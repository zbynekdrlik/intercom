//! The hub's non-Dante services stay off the Dante leg (intercom#25, dstack two-networks.md §4).
//!
//! The hub is both a Dante device (dlink, bound to the Dante address by dstack) and a LAN service:
//! the HTTP/PWA, the VBAN socket and the Janus RTP socket. dstack's ruling (intercom#25, option B):
//! those keep listening on all addresses and drop at once whatever arrives on the Dante leg, so a
//! LAN address that changes needs no rebind.
//!
//! The Dante leg is known two ways, the union of both:
//! - the dlink interface from the matrix (`[dlink] iface`): its ifindex, its IPv4 address and that
//!   address's subnet, re-resolved by [`DanteGuard::spawn_refresher`] (dlink brings the leg up and a
//!   DHCP lease can move it);
//! - `DANTE_SUBNET` (§4's per-app setting, CIDR, comma-separated), from the environment: the site's
//!   subnet is not in a public repo.
//!
//! [`DanteGuard`] is the one place that decides; the TCP accept loop ([`crate::http_serve`]), the
//! VBAN and Janus RTP receive loops ([`crate::pktinfo`]) and the NDI discovery ask it. A refusal is
//! counted per port and warned once per port (§4: the PC's layout is only the second line of
//! defence, so a refusal means that line was not there).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

/// The environment variable that names the Dante subnet(s), §4's per-app setting.
pub const DANTE_SUBNET_ENV: &str = "DANTE_SUBNET";

/// How often the dlink interface is re-resolved.
pub const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// An IPv4 subnet in CIDR form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Net {
    network: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    /// The subnet `addr/prefix`; host bits of `addr` are cleared. `prefix` above 32 is refused.
    pub fn new(addr: Ipv4Addr, prefix: u8) -> Result<Self> {
        if prefix > 32 {
            bail!("prefix /{prefix} is above /32");
        }
        Ok(Self {
            network: Ipv4Addr::from(u32::from(addr) & mask(prefix)),
            prefix,
        })
    }

    pub fn network(&self) -> Ipv4Addr {
        self.network
    }

    pub fn prefix(&self) -> u8 {
        self.prefix
    }

    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & mask(self.prefix) == u32::from(self.network)
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl FromStr for Ipv4Net {
    type Err = anyhow::Error;

    /// `a.b.c.d/n`, or a bare address as `/32`.
    fn from_str(s: &str) -> Result<Self> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (
                a,
                p.parse::<u8>()
                    .with_context(|| format!("bad prefix in '{s}'"))?,
            ),
            None => (s, 32),
        };
        let addr: Ipv4Addr = addr
            .parse()
            .with_context(|| format!("bad IPv4 address in '{s}'"))?;
        Ipv4Net::new(addr, prefix)
    }
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    }
}

/// Parse a `DANTE_SUBNET` value: one or more CIDRs separated by commas. Empty means none.
pub fn parse_subnets(value: &str) -> Result<Vec<Ipv4Net>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Ipv4Net::from_str)
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("{DANTE_SUBNET_ENV}='{value}'"))
}

/// What arrived with a datagram, from `IP_PKTINFO`: the interface it came in on and the
/// destination address in its IP header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketInfo {
    pub ifindex: u32,
    pub dst: Ipv4Addr,
}

/// The Dante leg as the hub knows it right now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DanteNets {
    /// The dlink interface's index, when it exists.
    pub ifindex: Option<u32>,
    /// Every IPv4 address on the dlink interface (a DHCP lease and a link-local one alike).
    pub iface_addrs: Vec<Ipv4Addr>,
    /// Every Dante subnet: the dlink interface's own and `DANTE_SUBNET`'s.
    pub nets: Vec<Ipv4Net>,
}

impl DanteNets {
    /// Whether `ip` (an IPv4-mapped IPv6 address counts as its IPv4) is a Dante address.
    pub fn is_dante(&self, ip: IpAddr) -> bool {
        let v4 = match ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4,
                None => return false,
            },
        };
        self.iface_addrs.contains(&v4) || self.nets.iter().any(|n| n.contains(v4))
    }

    /// A TCP connection is refused when its local address or its peer is a Dante address. Linux
    /// accepts a packet for any local address on any interface, so a Dante host that sends to the
    /// LAN address over the Dante leg has a LAN local address; its own address gives it away
    /// (§4: no address in `<dante>` is ever used for non-Dante traffic).
    pub fn refuses_connection(&self, local: SocketAddr, peer: SocketAddr) -> bool {
        self.is_dante(local.ip()) || self.is_dante(peer.ip())
    }

    /// A datagram is refused when it came in on the dlink interface, was sent to a Dante address,
    /// or comes from one. Without packet info only the sender's address can be judged here; the
    /// guard refuses such a datagram outright while it is active ([`DanteGuard::check_datagram`]).
    pub fn refuses_datagram(&self, info: Option<PacketInfo>, from: SocketAddr) -> bool {
        let from_dante = self.is_dante(from.ip());
        match info {
            Some(i) => {
                from_dante || self.ifindex == Some(i.ifindex) || self.is_dante(IpAddr::V4(i.dst))
            }
            None => from_dante,
        }
    }
}

/// How many distinct ports get their own refusal counter (the hub has three: HTTP, VBAN, RTP).
pub const REFUSAL_PORT_SLOTS: usize = 8;

/// The per-port refusal counts, lock-free: the real-time hub-mix thread reads them once a second
/// and must never wait on a receive task. A port takes a free slot on its first refusal; past
/// [`REFUSAL_PORT_SLOTS`] ports the rest are counted under port 0. The first refusal on a port is
/// warned once.
#[derive(Debug, Default)]
pub struct Refusals {
    /// 0 = free, else the port + 1.
    ports: [AtomicU32; REFUSAL_PORT_SLOTS],
    counts: [AtomicU64; REFUSAL_PORT_SLOTS],
    other: AtomicU64,
}

impl Refusals {
    /// Count one refusal on `port`; true when it is the port's first.
    pub fn record(&self, port: u16) -> bool {
        let key = u32::from(port) + 1;
        for (slot, count) in self.ports.iter().zip(&self.counts) {
            let owner = match slot.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => key,
                Err(current) => current,
            };
            if owner == key {
                return count.fetch_add(1, Ordering::Relaxed) == 0;
            }
        }
        self.other.fetch_add(1, Ordering::Relaxed) == 0
    }

    pub fn snapshot(&self) -> BTreeMap<u16, u64> {
        let mut map = BTreeMap::new();
        for (slot, count) in self.ports.iter().zip(&self.counts) {
            let key = slot.load(Ordering::Acquire);
            let n = count.load(Ordering::Relaxed);
            if key != 0 && n != 0 {
                map.insert((key - 1) as u16, n);
            }
        }
        let other = self.other.load(Ordering::Relaxed);
        if other != 0 {
            *map.entry(0).or_insert(0) += other;
        }
        map
    }
}

/// The hub's one decision point for "is this the Dante leg".
#[derive(Debug)]
pub struct DanteGuard {
    iface: Option<String>,
    configured: Vec<Ipv4Net>,
    nets: RwLock<DanteNets>,
    refusals: Refusals,
}

impl DanteGuard {
    /// A guard for the dlink interface `iface` (if any) and the `configured` subnets, resolved now.
    pub fn new(iface: Option<String>, configured: Vec<Ipv4Net>) -> Arc<Self> {
        let nets = build_nets(iface.as_deref(), &configured, resolve_iface);
        Arc::new(Self {
            iface,
            configured,
            nets: RwLock::new(nets),
            refusals: Refusals::default(),
        })
    }

    /// [`DanteGuard::new`] with the subnets read from `DANTE_SUBNET`. A malformed value is an error
    /// (the hub refuses to start rather than run without the filter it was told to apply).
    pub fn from_env(iface: Option<String>) -> Result<Arc<Self>> {
        let configured = match std::env::var(DANTE_SUBNET_ENV) {
            Ok(v) => parse_subnets(&v)?,
            Err(std::env::VarError::NotPresent) => Vec::new(),
            Err(e) => return Err(anyhow!("{DANTE_SUBNET_ENV}: {e}")),
        };
        Ok(Self::new(iface, configured))
    }

    /// Whether the guard filters anything: a dlink interface or a configured subnet.
    pub fn active(&self) -> bool {
        self.iface.is_some() || !self.configured.is_empty()
    }

    pub fn iface(&self) -> Option<&str> {
        self.iface.as_deref()
    }

    /// The Dante leg as known right now.
    pub fn current(&self) -> DanteNets {
        self.nets.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-resolve the dlink interface; returns the new view when it changed.
    pub fn refresh(&self) -> Option<DanteNets> {
        let fresh = build_nets(self.iface.as_deref(), &self.configured, resolve_iface);
        let mut cur = self.nets.write().unwrap_or_else(|e| e.into_inner());
        if *cur == fresh {
            return None;
        }
        *cur = fresh.clone();
        Some(fresh)
    }

    /// Whether `ip` is a Dante address right now.
    pub fn is_dante(&self, ip: IpAddr) -> bool {
        self.nets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_dante(ip)
    }

    /// Decide an accepted TCP connection; a refusal is counted and logged. True = refuse it.
    pub fn check_connection(&self, local: SocketAddr, peer: SocketAddr) -> bool {
        let refused = self
            .nets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .refuses_connection(local, peer);
        if refused {
            self.note_refusal("tcp", local.port(), peer);
        }
        refused
    }

    /// Decide a received datagram on `port`; a refusal is counted and logged. True = drop it.
    /// While the guard is active a datagram without packet info is refused (fail closed: the
    /// sockets are IPv4 with `IP_PKTINFO`, so a missing one is a truncated control message).
    pub fn check_datagram(&self, port: u16, info: Option<PacketInfo>, from: SocketAddr) -> bool {
        let refused = (self.active() && info.is_none())
            || self
                .nets
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .refuses_datagram(info, from);
        if refused {
            self.note_refusal("udp", port, from);
        }
        refused
    }

    /// The refusal counts per local port (`/api/state` `dante_refusals`).
    pub fn refusals(&self) -> BTreeMap<u16, u64> {
        self.refusals.snapshot()
    }

    fn note_refusal(&self, proto: &str, port: u16, peer: SocketAddr) {
        if self.refusals.record(port) {
            tracing::warn!(
                proto,
                port,
                %peer,
                iface = self.iface.as_deref().unwrap_or("-"),
                "dante-guard: refused traffic that arrived on the Dante leg (counted per port in /api/state dante_refusals; further refusals on this port are not logged)"
            );
        }
    }

    /// Re-resolve the dlink interface every [`REFRESH_EVERY`] and log each change. Only when there
    /// is an interface to follow.
    pub fn spawn_refresher(self: &Arc<Self>) {
        if self.iface.is_none() {
            return;
        }
        let guard = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REFRESH_EVERY);
            loop {
                tick.tick().await;
                if let Some(now) = guard.refresh() {
                    tracing::info!(
                        iface = guard.iface.as_deref().unwrap_or("-"),
                        ifindex = ?now.ifindex,
                        addrs = ?now.iface_addrs,
                        nets = %format_nets(&now.nets),
                        "dante-guard: the Dante leg changed"
                    );
                }
            }
        });
    }

    /// One log line naming what the guard filters.
    pub fn log_start(&self) {
        let now = self.current();
        if !self.active() {
            tracing::info!(
                "dante-guard: no dlink interface and no {DANTE_SUBNET_ENV}: nothing to filter"
            );
            return;
        }
        tracing::info!(
            iface = self.iface.as_deref().unwrap_or("-"),
            ifindex = ?now.ifindex,
            addrs = ?now.iface_addrs,
            nets = %format_nets(&now.nets),
            "dante-guard: the HTTP, VBAN and Janus RTP sockets refuse traffic on the Dante leg"
        );
    }
}

fn format_nets(nets: &[Ipv4Net]) -> String {
    if nets.is_empty() {
        return "-".to_string();
    }
    nets.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// The resolved view of one interface: its index and every IPv4 address with its subnet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IfaceInfo {
    pub ifindex: u32,
    pub addrs: Vec<(Ipv4Addr, Ipv4Net)>,
}

/// Build the view from an interface resolver (pure given `resolve`).
pub fn build_nets(
    iface: Option<&str>,
    configured: &[Ipv4Net],
    resolve: impl Fn(&str) -> Option<IfaceInfo>,
) -> DanteNets {
    let mut nets = DanteNets {
        nets: configured.to_vec(),
        ..DanteNets::default()
    };
    if let Some(info) = iface.and_then(resolve) {
        nets.ifindex = Some(info.ifindex);
        for (addr, net) in info.addrs {
            nets.iface_addrs.push(addr);
            if !nets.nets.contains(&net) {
                nets.nets.push(net);
            }
        }
    }
    nets
}

/// Resolve `name` with `if_nametoindex` and `getifaddrs`. None when the interface does not exist.
pub fn resolve_iface(name: &str) -> Option<IfaceInfo> {
    let cname = std::ffi::CString::new(name).ok()?;
    // SAFETY: a valid NUL-terminated string; the call only reads it.
    let ifindex = unsafe { libc::if_nametoindex(cname.as_ptr()) };
    if ifindex == 0 {
        return None;
    }
    Some(IfaceInfo {
        ifindex,
        addrs: iface_ipv4s(name),
    })
}

fn iface_ipv4s(name: &str) -> Vec<(Ipv4Addr, Ipv4Net)> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list we free below with freeifaddrs.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        tracing::warn!(iface = name, error = %std::io::Error::last_os_error(), "dante-guard: getifaddrs failed");
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: `cur` is a node of the list getifaddrs returned and is not freed yet.
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_name.is_null() || ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
            continue;
        }
        // SAFETY: a NUL-terminated name owned by the list.
        let ifa_name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) };
        if ifa_name.to_bytes() != name.as_bytes() {
            continue;
        }
        // SAFETY: ifa_addr points at a sockaddr whose family field is valid to read.
        if i32::from(unsafe { (*ifa.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: AF_INET: both pointers are sockaddr_in.
        let (addr, netmask) = unsafe {
            let a = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let m = &*(ifa.ifa_netmask as *const libc::sockaddr_in);
            (
                Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)),
                u32::from_be(m.sin_addr.s_addr),
            )
        };
        let prefix = netmask.leading_ones() as u8;
        if let Ok(net) = Ipv4Net::new(addr, prefix) {
            found.push((addr, net));
        }
    }
    // SAFETY: the list getifaddrs allocated, freed once.
    unsafe { libc::freeifaddrs(head) };
    found
}

/// Refuse a configured listen address inside the Dante leg: whatever the hub binds (and tells
/// Janus, for the RTP bind) must never be a Dante address (§4).
///
/// While the guard is active the bind must be IPv4: only an IPv4 socket's datagrams carry the
/// ingress interface, and native IPv6 traffic on the Dante iface could not be told apart.
pub fn check_bind(guard: &DanteGuard, what: &str, addr: SocketAddr) -> Result<()> {
    if guard.active() && addr.is_ipv6() {
        bail!("{what} {addr} is IPv6: with a Dante leg the hub binds IPv4 only, so every datagram carries its ingress interface (intercom#25)");
    }
    if guard.is_dante(addr.ip()) {
        bail!("{what} {addr} is a Dante address: a non-Dante service never binds the Dante leg (intercom#25, two-networks §4)");
    }
    Ok(())
}
