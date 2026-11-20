//! A fake TURN server in the test process (intercom#42 phase 3), sans-IO like the hub's client:
//! `handle` takes one datagram a client sent (`client` = its 5-tuple: the hub's socket epoch) and
//! returns the server's answers. The fake tracks ONE current allocation (`alloc_client`); an
//! Allocate from another 5-tuple makes a new one (the old one is left to expire, untracked), any
//! other request from a 5-tuple that holds no current allocation is 437. It speaks the
//! long-term credential mechanism (401 with REALM + NONCE, then MESSAGE-INTEGRITY checked against
//! the user table), Allocate / Refresh (LIFETIME 0 = release) / CreatePermission / ChannelBind, Send
//! indications and ChannelData from the client, and builds Data indications and ChannelData to it.
//! RFC 5766 rules the hub must follow are enforced: a request on an existing allocation signed by
//! another username is 441, an Allocate while one exists is 437, a Send to an IP without a
//! permission is dropped, a channel number bound to another peer (or a peer bound to another
//! number) is 400. A STUN Binding request (the client's keepalive) gets its success answer.
//! Knobs make it fail or go silent.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use rtc_stun::attributes::{ATTR_MESSAGE_INTEGRITY, ATTR_NONCE, ATTR_REALM, ATTR_USERNAME};
use rtc_stun::error_code::{ErrorCode, ErrorCodeAttribute};
use rtc_stun::fingerprint::FINGERPRINT;
use rtc_stun::integrity::MessageIntegrity;
use rtc_stun::message::{
    Getter, Message, MessageType, Setter, CLASS_ERROR_RESPONSE, CLASS_INDICATION, CLASS_REQUEST,
    CLASS_SUCCESS_RESPONSE, METHOD_ALLOCATE, METHOD_BINDING, METHOD_CHANNEL_BIND,
    METHOD_CREATE_PERMISSION, METHOD_DATA, METHOD_REFRESH, METHOD_SEND,
};
use rtc_stun::textattrs::TextAttribute;
use rtc_stun::xoraddr::XorMappedAddress;
use rtc_turn::proto::chandata::ChannelData;
use rtc_turn::proto::channum::ChannelNumber;
use rtc_turn::proto::data::Data;
use rtc_turn::proto::lifetime::Lifetime;
use rtc_turn::proto::peeraddr::PeerAddress;
use rtc_turn::proto::relayaddr::RelayedAddress;

pub const REALM: &str = "fake.turn";

/// One request the fake saw (method name, username), in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: &'static str,
    pub user: Option<String>,
    /// LIFETIME of a Refresh.
    pub lifetime: Option<u64>,
}

pub struct FakeTurn {
    pub addr: SocketAddr,
    pub relayed: SocketAddr,
    pub mapped: SocketAddr,
    pub lifetime_s: u64,
    users: HashMap<String, String>,
    nonce: String,
    /// The username the allocation belongs to (None = no allocation).
    pub alloc_user: Option<String>,
    pub perms: HashSet<IpAddr>,
    pub channels: HashMap<u16, SocketAddr>,
    /// (peer, payload, came as ChannelData) the client relayed out.
    pub relayed_out: Vec<(SocketAddr, Vec<u8>, bool)>,
    pub seen: Vec<Seen>,
    /// Datagrams received at all (retransmissions included).
    pub datagrams: usize,
    /// Answer nothing.
    pub silent: bool,
    /// Answer nothing to these clients (5-tuples).
    pub silent_clients: HashSet<u32>,
    /// The client (5-tuple) the current allocation belongs to.
    pub alloc_client: Option<u32>,
    /// Answer every Allocate with this error code.
    pub fail_allocate: Option<u16>,
    /// Answer every Refresh with this error code.
    pub fail_refresh: Option<u16>,
    /// Answer every CreatePermission with this error code.
    pub fail_permission: Option<u16>,
    /// Answer the next authenticated request with 438 and a fresh nonce.
    pub stale_next: bool,
    /// Answer the next ChannelBind with this error code.
    pub fail_channel_bind_once: Option<u16>,
    /// Every ChannelBind the fake accepted: (number, peer), in order.
    pub binds: Vec<(u16, SocketAddr)>,
    /// Every release (Refresh LIFETIME 0): (client, username).
    pub released: Vec<(u32, String)>,
}

fn crypto() -> std::sync::Arc<dyn rtc_turn::crypto::RTCCryptoProvider> {
    rtc_turn::crypto::default_provider().unwrap()
}

fn method_name(m: &Message) -> &'static str {
    match m.typ.method {
        METHOD_ALLOCATE => "allocate",
        METHOD_REFRESH => "refresh",
        METHOD_CREATE_PERMISSION => "permission",
        METHOD_CHANNEL_BIND => "channel-bind",
        METHOD_SEND => "send",
        METHOD_BINDING => "binding",
        _ => "other",
    }
}

impl FakeTurn {
    pub fn new(addr: SocketAddr, relayed: SocketAddr, users: &[(&str, &str)]) -> Self {
        FakeTurn {
            addr,
            relayed,
            mapped: "198.51.100.7:40000".parse().unwrap(),
            lifetime_s: 600,
            users: users
                .iter()
                .map(|(u, p)| (u.to_string(), p.to_string()))
                .collect(),
            nonce: "nonce-1".into(),
            alloc_user: None,
            perms: HashSet::new(),
            channels: HashMap::new(),
            relayed_out: Vec::new(),
            seen: Vec::new(),
            datagrams: 0,
            silent: false,
            silent_clients: HashSet::new(),
            alloc_client: None,
            fail_allocate: None,
            fail_refresh: None,
            fail_permission: None,
            stale_next: false,
            fail_channel_bind_once: None,
            binds: Vec::new(),
            released: Vec::new(),
        }
    }

    /// How many requests of `method` the fake answered or refused.
    pub fn count(&self, method: &str) -> usize {
        self.seen.iter().filter(|s| s.method == method).count()
    }

    /// The server lost the allocation (a restart, an expiry): the next refresh gets 437.
    pub fn forget_allocation(&mut self) {
        self.alloc_user = None;
        self.alloc_client = None;
        self.perms.clear();
        self.channels.clear();
    }

    fn key(&self, user: &str) -> Option<Vec<u8>> {
        let pass = self.users.get(user)?;
        let c = crypto();
        MessageIntegrity::long_term_integrity_key(
            user.to_string(),
            REALM.to_string(),
            pass.clone(),
            c.crypto(),
        )
        .ok()
    }

    fn error(&self, req: &Message, code: u16, with_auth_hint: bool) -> Vec<u8> {
        let mut m = Message::new();
        let mut setters: Vec<Box<dyn Setter>> = vec![
            Box::new(req.transaction_id),
            Box::new(MessageType::new(req.typ.method, CLASS_ERROR_RESPONSE)),
            Box::new(ErrorCodeAttribute {
                code: ErrorCode(code),
                reason: format!("fake {code}").into_bytes(),
            }),
        ];
        if with_auth_hint {
            setters.push(Box::new(TextAttribute::new(ATTR_REALM, REALM.into())));
            setters.push(Box::new(TextAttribute::new(ATTR_NONCE, self.nonce.clone())));
        }
        setters.push(Box::new(FINGERPRINT));
        m.build(&setters).unwrap();
        m.raw
    }

    /// The relayed address the fake gives `client`'s allocation (one per 5-tuple).
    pub fn relayed_of(&self, client: u32) -> SocketAddr {
        SocketAddr::new(self.relayed.ip(), self.relayed.port() + client as u16)
    }

    /// `client`'s datagram in, the server's answers out.
    pub fn handle(&mut self, client: u32, data: &[u8]) -> Vec<Vec<u8>> {
        self.datagrams += 1;
        if self.silent || self.silent_clients.contains(&client) {
            return Vec::new();
        }
        let current = self.alloc_user.is_some() && self.alloc_client == Some(client);
        if ChannelData::is_channel_data(data) && !rtc_stun::message::is_stun_message(data) {
            let mut cd = ChannelData {
                raw: data.to_vec(),
                ..Default::default()
            };
            if current && cd.decode().is_ok() {
                if let Some(peer) = self.channels.get(&cd.number.0).copied() {
                    if self.perms.contains(&peer.ip()) {
                        self.relayed_out.push((peer, cd.data, true));
                    }
                }
            }
            return Vec::new();
        }
        let mut m = Message::new();
        m.raw = data.to_vec();
        if m.decode().is_err() {
            return Vec::new();
        }
        if m.typ.class == CLASS_INDICATION && m.typ.method == METHOD_SEND {
            let mut peer = PeerAddress::default();
            let mut d = Data::default();
            if peer.get_from(&m).is_ok() && d.get_from(&m).is_ok() {
                let peer = SocketAddr::new(peer.ip, peer.port);
                if current && self.perms.contains(&peer.ip()) {
                    self.relayed_out.push((peer, d.0, false));
                }
            }
            return Vec::new();
        }
        if m.typ.class != CLASS_REQUEST {
            return Vec::new();
        }
        let user = TextAttribute::get_from_as(&m, ATTR_USERNAME)
            .ok()
            .map(|t| t.text);
        let mut lifetime = None;
        if m.typ.method == METHOD_REFRESH {
            let mut l = Lifetime::default();
            if l.get_from(&m).is_ok() {
                lifetime = Some(l.0.as_secs());
            }
        }
        self.seen.push(Seen {
            method: method_name(&m),
            user: user.clone(),
            lifetime,
        });
        // A Binding request (the keepalive): answered with the mapped address, no credential.
        if m.typ.method == METHOD_BINDING {
            let mut r = Message::new();
            r.build(&[
                Box::new(m.transaction_id),
                Box::new(MessageType::new(METHOD_BINDING, CLASS_SUCCESS_RESPONSE)),
                Box::new(XorMappedAddress {
                    ip: self.mapped.ip(),
                    port: self.mapped.port(),
                }),
                Box::new(FINGERPRINT),
            ])
            .unwrap();
            return vec![r.raw];
        }
        // Long-term credentials: no integrity -> 401 with the realm and nonce.
        if !m.contains(ATTR_MESSAGE_INTEGRITY) {
            return vec![self.error(&m, 401, true)];
        }
        let Some(user) = user else {
            return vec![self.error(&m, 400, false)];
        };
        let Some(key) = self.key(&user) else {
            return vec![self.error(&m, 401, true)];
        };
        let c = crypto();
        let mut check = m.clone();
        if MessageIntegrity::check(&mut check, &key, c.crypto()).is_err() {
            return vec![self.error(&m, 401, true)];
        }
        let nonce = TextAttribute::get_from_as(&m, ATTR_NONCE)
            .map(|t| t.text)
            .unwrap_or_default();
        if self.stale_next || nonce != self.nonce {
            self.stale_next = false;
            self.nonce = format!("{}x", self.nonce);
            return vec![self.error(&m, 438, true)];
        }
        let class = CLASS_SUCCESS_RESPONSE;
        match m.typ.method {
            METHOD_ALLOCATE => {
                if let Some(code) = self.fail_allocate {
                    return vec![self.error(&m, code, false)];
                }
                if current {
                    return vec![self.error(&m, 437, false)];
                }
                // A new 5-tuple: a new allocation (the previous one is left to expire).
                self.alloc_user = Some(user);
                self.alloc_client = Some(client);
                self.perms.clear();
                self.channels.clear();
                let relayed = self.relayed_of(client);
                let mut r = Message::new();
                r.build(&[
                    Box::new(m.transaction_id),
                    Box::new(MessageType::new(METHOD_ALLOCATE, class)),
                    Box::new(RelayedAddress {
                        ip: relayed.ip(),
                        port: relayed.port(),
                    }),
                    Box::new(XorMappedAddress {
                        ip: self.mapped.ip(),
                        port: self.mapped.port(),
                    }),
                    Box::new(Lifetime(Duration::from_secs(self.lifetime_s))),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
                vec![r.raw]
            }
            METHOD_REFRESH => {
                if let Some(code) = self.fail_refresh {
                    return vec![self.error(&m, code, false)];
                }
                let granted = lifetime.unwrap_or(self.lifetime_s).min(self.lifetime_s);
                if granted == 0 && !current {
                    // The release of an older allocation (another 5-tuple): it goes, the current
                    // one stays.
                    self.released.push((client, user.clone()));
                } else {
                    match &self.alloc_user {
                        _ if !current => return vec![self.error(&m, 437, false)],
                        Some(owner) if *owner != user => return vec![self.error(&m, 441, false)],
                        _ => {}
                    }
                    if granted == 0 {
                        self.released.push((client, user.clone()));
                        self.forget_allocation();
                    }
                }
                let mut r = Message::new();
                r.build(&[
                    Box::new(m.transaction_id),
                    Box::new(MessageType::new(METHOD_REFRESH, class)),
                    Box::new(Lifetime(Duration::from_secs(granted))),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
                vec![r.raw]
            }
            METHOD_CREATE_PERMISSION => {
                if let Some(code) = self.fail_permission {
                    return vec![self.error(&m, code, false)];
                }
                match &self.alloc_user {
                    _ if !current => return vec![self.error(&m, 437, false)],
                    Some(owner) if *owner != user => return vec![self.error(&m, 441, false)],
                    _ => {}
                }
                for a in &m.attributes.0 {
                    if a.typ == rtc_stun::attributes::ATTR_XOR_PEER_ADDRESS {
                        let mut one = Message::new();
                        one.build(&[Box::new(m.transaction_id)]).unwrap();
                        one.add(a.typ, &a.value);
                        let mut p = PeerAddress::default();
                        if p.get_from(&one).is_ok() {
                            self.perms.insert(p.ip);
                        }
                    }
                }
                let mut r = Message::new();
                r.build(&[
                    Box::new(m.transaction_id),
                    Box::new(MessageType::new(METHOD_CREATE_PERMISSION, class)),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
                vec![r.raw]
            }
            METHOD_CHANNEL_BIND => {
                match &self.alloc_user {
                    _ if !current => return vec![self.error(&m, 437, false)],
                    Some(owner) if *owner != user => return vec![self.error(&m, 441, false)],
                    _ => {}
                }
                let mut p = PeerAddress::default();
                let mut n = ChannelNumber::default();
                if p.get_from(&m).is_err() || n.get_from(&m).is_err() {
                    return vec![self.error(&m, 400, false)];
                }
                let peer = SocketAddr::new(p.ip, p.port);
                if let Some(code) = self.fail_channel_bind_once.take() {
                    return vec![self.error(&m, code, false)];
                }
                // RFC 8656 §12: a number bound to another peer, or a peer bound to another number,
                // is refused while the binding lives.
                let clash = self
                    .channels
                    .iter()
                    .any(|(num, p)| (*num == n.0) != (*p == peer));
                if clash {
                    return vec![self.error(&m, 400, false)];
                }
                // A channel binding installs a permission too (RFC 5766 §11.2).
                self.perms.insert(peer.ip());
                self.channels.insert(n.0, peer);
                self.binds.push((n.0, peer));
                let mut r = Message::new();
                r.build(&[
                    Box::new(m.transaction_id),
                    Box::new(MessageType::new(METHOD_CHANNEL_BIND, class)),
                    Box::new(FINGERPRINT),
                ])
                .unwrap();
                vec![r.raw]
            }
            _ => vec![self.error(&m, 400, false)],
        }
    }

    /// A datagram `peer` sent to the relayed address, as the server hands it to the client: as
    /// ChannelData when a channel is bound to that peer, else a Data indication. `None` when the
    /// server drops it (no allocation, or no permission for the peer's IP).
    pub fn deliver_from(&self, peer: SocketAddr, payload: &[u8]) -> Option<Vec<u8>> {
        if self.alloc_user.is_none() || !self.perms.contains(&peer.ip()) {
            return None;
        }
        if let Some((&n, _)) = self.channels.iter().find(|(_, p)| **p == peer) {
            let mut cd = ChannelData {
                data: payload.to_vec(),
                number: ChannelNumber(n),
                ..Default::default()
            };
            cd.encode();
            return Some(cd.raw);
        }
        Some(data_indication(peer, payload))
    }
}

/// A Data indication from `peer` (what the server sends when no channel is bound).
pub fn data_indication(peer: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut m = Message::new();
    m.build(&[
        Box::new(rtc_stun::message::TransactionId::new()),
        Box::new(MessageType::new(METHOD_DATA, CLASS_INDICATION)),
        Box::new(PeerAddress {
            ip: peer.ip(),
            port: peer.port(),
        }),
        Box::new(Data(payload.to_vec())),
        Box::new(FINGERPRINT),
    ])
    .unwrap();
    m.raw
}

/// ChannelData on channel `number`.
pub fn channel_data(number: u16, payload: &[u8]) -> Vec<u8> {
    let mut cd = ChannelData {
        data: payload.to_vec(),
        number: ChannelNumber(number),
        ..Default::default()
    };
    cd.encode();
    cd.raw
}
