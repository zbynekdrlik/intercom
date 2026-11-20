//! The hub's own TURN relay (intercom#42 phase 3), sans-IO, against a fake TURN server in this
//! process (`fake_turn`): the allocation with the long-term credential (401 -> signed Allocate), the
//! refresh before the lifetime ends, a credential rotation (release + re-allocate, deferred while a
//! phone is on the relay, never past the old credential's expiry), the failure back-off, the
//! retransmissions, the permissions (IPv4, bounded, refreshed only while wanted), the channel bind,
//! and the demux / wrap: Data indications and ChannelData unwrapped to (peer, payload), sends
//! wrapped as Send indications and then ChannelData. No datagram ever goes to a peer: everything
//! the relay emits is addressed to the TURN server.

mod fake_turn;

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use fake_turn::{channel_data, data_indication, FakeTurn};
use intercom_hub::webrtc_turn::{
    turn_udp_server, TurnCreds, TurnOptions, TurnPhase, TurnRelay, TURN_BACKOFF_MAX,
    TURN_BACKOFF_MIN, TURN_PERMISSION_REFRESH, TURN_ROTATE_MARGIN, TURN_TX_TIMEOUT,
};

const SERVER: &str = "203.0.113.10:3478";
const RELAYED: &str = "203.0.113.10:50001";

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn creds(user: &str, pass: &str, expires_at: Instant) -> TurnCreds {
    TurnCreds {
        server: sa(SERVER),
        username: user.into(),
        password: pass.into(),
        expires_at,
    }
}

fn opts() -> TurnOptions {
    TurnOptions {
        max_permissions: 8,
        max_channels: 4,
        refresh_cap: None,
    }
}

fn fake() -> FakeTurn {
    FakeTurn::new(
        sa(SERVER),
        sa(RELAYED),
        &[("alice", "pw-a"), ("bob", "pw-b")],
    )
}

/// Hand every datagram the relay wants to send to the fake and every answer back, until quiet.
/// Returns the (peer, payload) pairs the answers carried (none expected for responses).
fn pump(relay: &mut TurnRelay, srv: &mut FakeTurn, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut got = Vec::new();
    for _ in 0..100 {
        let mut any = false;
        while let Some(d) = relay.poll_transmit() {
            any = true;
            for r in srv.handle(&d) {
                if let Some(p) = relay.handle_datagram(&r, now) {
                    got.push(p);
                }
            }
        }
        if !any {
            return got;
        }
    }
    panic!("the relay never went quiet");
}

/// A relay allocated with alice at `t0`.
fn allocated(t0: Instant, o: TurnOptions) -> (TurnRelay, FakeTurn) {
    let mut relay = TurnRelay::new(o).unwrap();
    let mut srv = fake();
    relay.set_credentials(creds("alice", "pw-a", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    (relay, srv)
}

#[test]
fn it_waits_for_credentials_and_sends_nothing_before() {
    let t0 = Instant::now();
    let mut relay = TurnRelay::new(opts()).unwrap();
    relay.handle_timeout(t0, false);
    assert_eq!(relay.phase(), TurnPhase::Waiting);
    assert!(relay.poll_transmit().is_none());
    assert!(relay.relayed().is_none());
    let f = relay.facet(t0);
    assert_eq!(f.state, "waiting");
    assert_eq!(f.relayed, None);
}

#[test]
fn it_allocates_with_the_long_term_credential() {
    let t0 = Instant::now();
    let (relay, srv) = allocated(t0, opts());
    // The unauthenticated Allocate got 401 + realm/nonce, the signed one the allocation.
    assert_eq!(srv.count("allocate"), 2);
    assert_eq!(srv.alloc_user.as_deref(), Some("alice"));
    assert_eq!(relay.relayed(), Some(sa(RELAYED)));
    assert!(relay.is_server(sa(SERVER)));
    assert!(!relay.is_server(sa("203.0.113.10:3479")));
    let f = relay.facet(t0);
    assert_eq!(f.state, "allocated");
    assert_eq!(f.relayed.as_deref(), Some(RELAYED));
    assert_eq!(f.server.as_deref(), Some(SERVER));
    assert_eq!(f.lifetime_left_s, Some(600));
    assert_eq!((f.allocations, f.refreshes, f.failures), (1, 0, 0));
    // The facet and the Debug print never carry the password.
    let shown = format!("{f:?} {:?}", creds("alice", "pw-a", t0));
    assert!(!shown.contains("pw-a"), "{shown}");
}

#[test]
fn it_refreshes_at_half_the_lifetime_and_the_lifetime_left_counts_down() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let t = t0 + Duration::from_secs(299);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(srv.count("refresh"), 0);
    assert_eq!(relay.facet(t).lifetime_left_s, Some(301));
    let t = t0 + Duration::from_secs(301);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(srv.count("refresh"), 1);
    let f = relay.facet(t);
    assert_eq!(f.refreshes, 1);
    assert_eq!(f.lifetime_left_s, Some(600));
    assert_eq!(f.state, "allocated");
    // The refresh asks for a lifetime, never 0 (0 releases).
    assert!(srv
        .seen
        .iter()
        .any(|s| s.method == "refresh" && s.lifetime == Some(600)));
}

#[test]
fn the_refresh_cap_refreshes_sooner() {
    let t0 = Instant::now();
    let mut o = opts();
    o.refresh_cap = Some(Duration::from_secs(1));
    let (mut relay, mut srv) = allocated(t0, o);
    for k in 1..=3u64 {
        let t = t0 + Duration::from_millis(1001 * k);
        relay.handle_timeout(t, false);
        pump(&mut relay, &mut srv, t);
    }
    assert_eq!(srv.count("refresh"), 3);
    assert_eq!(relay.facet(t0).refreshes, 3);
}

#[test]
fn a_stale_nonce_is_retried_with_the_new_one() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    srv.stale_next = true;
    let t = t0 + Duration::from_secs(301);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    // 438, then the same refresh with the fresh nonce.
    assert_eq!(srv.count("refresh"), 2);
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    assert_eq!(relay.facet(t).failures, 0);
    assert_eq!(relay.facet(t).refreshes, 1);
}

#[test]
fn a_lost_allocation_is_reallocated() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    srv.forget_allocation();
    let t = t0 + Duration::from_secs(301);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    // The refresh got 437: the allocation is gone, back off and allocate again.
    assert_eq!(relay.phase(), TurnPhase::Backoff);
    assert!(relay.relayed().is_none());
    let f = relay.facet(t);
    assert_eq!(f.failures, 1);
    assert!(
        f.last_error.as_deref().unwrap_or("").contains("437"),
        "{f:?}"
    );
    let t = t + TURN_BACKOFF_MIN;
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    assert_eq!(relay.facet(t).allocations, 2);
}

#[test]
fn a_failing_allocate_backs_off_exponentially_up_to_the_cap() {
    let t0 = Instant::now();
    let mut relay = TurnRelay::new(opts()).unwrap();
    let mut srv = fake();
    srv.fail_allocate = Some(486); // Allocation Quota Reached
    relay.set_credentials(creds("alice", "pw-a", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.phase(), TurnPhase::Backoff);
    let mut t = t0;
    let mut wait = TURN_BACKOFF_MIN;
    let mut waits = Vec::new();
    for _ in 0..10 {
        let before = srv.count("allocate");
        // Nothing before the back-off ends.
        relay.handle_timeout(t + wait - Duration::from_millis(1), false);
        pump(&mut relay, &mut srv, t + wait - Duration::from_millis(1));
        assert_eq!(
            srv.count("allocate"),
            before,
            "no attempt inside the back-off"
        );
        let due = relay.poll_timeout().expect("a retry is scheduled");
        assert_eq!(due, t + wait);
        t += wait;
        relay.handle_timeout(t, false);
        pump(&mut relay, &mut srv, t);
        assert!(
            srv.count("allocate") > before,
            "a retry at the end of the back-off"
        );
        assert_eq!(relay.phase(), TurnPhase::Backoff);
        waits.push(wait);
        wait = (wait * 2).min(TURN_BACKOFF_MAX);
    }
    assert_eq!(waits[0], TURN_BACKOFF_MIN);
    assert_eq!(waits[1], TURN_BACKOFF_MIN * 2);
    assert_eq!(*waits.last().unwrap(), TURN_BACKOFF_MAX);
    assert!(relay.facet(t).last_error.unwrap().contains("486"));
    // The server heals: the next retry allocates and the back-off starts over.
    srv.fail_allocate = None;
    t += TURN_BACKOFF_MAX;
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    srv.forget_allocation();
    let t2 = t + Duration::from_secs(301);
    relay.handle_timeout(t2, false);
    pump(&mut relay, &mut srv, t2);
    assert_eq!(relay.phase(), TurnPhase::Backoff);
    assert_eq!(relay.poll_timeout(), Some(t2 + TURN_BACKOFF_MIN));
}

#[test]
fn a_silent_server_gets_retransmissions_then_the_allocate_times_out() {
    let t0 = Instant::now();
    let mut relay = TurnRelay::new(opts()).unwrap();
    let mut srv = fake();
    srv.silent = true;
    relay.set_credentials(creds("alice", "pw-a", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.phase(), TurnPhase::Allocating);
    let mut t = t0;
    while t < t0 + TURN_TX_TIMEOUT {
        t += Duration::from_millis(100);
        relay.handle_timeout(t, false);
        pump(&mut relay, &mut srv, t);
    }
    // 0 / 0.5 / 1.5 / 3.5 s: four sends, then given up.
    assert_eq!(srv.datagrams, 4);
    assert_eq!(relay.phase(), TurnPhase::Backoff);
    assert!(relay.facet(t).last_error.unwrap().contains("no answer"));
}

#[test]
fn a_rotation_releases_and_reallocates_with_the_new_username_when_idle() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let t = t0 + Duration::from_secs(10);
    relay.set_credentials(creds("bob", "pw-b", t + Duration::from_secs(86_400)), t);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    // The old allocation was released with ALICE's signature (bob's would be 441), then bob
    // allocated.
    let release = srv
        .seen
        .iter()
        .find(|s| s.method == "refresh" && s.lifetime == Some(0))
        .expect("a release");
    assert_eq!(release.user.as_deref(), Some("alice"));
    assert_eq!(srv.alloc_user.as_deref(), Some("bob"));
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    assert_eq!(relay.facet(t).allocations, 2);
    // Later requests are bob's.
    let t = t + Duration::from_secs(301);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    let last = srv.seen.last().unwrap();
    assert_eq!(
        (last.method, last.user.as_deref()),
        ("refresh", Some("bob"))
    );
    assert_eq!(relay.phase(), TurnPhase::Allocated);
}

#[test]
fn a_release_the_server_drops_late_is_waited_for_not_counted_as_a_failure() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    srv.release_lag = 3; // coturn: the released allocation goes on its next timer pass
    relay.set_credentials(creds("bob", "pw-b", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(
        relay.phase(),
        TurnPhase::Releasing,
        "437 right after the release: settling"
    );
    let mut t = t0;
    for _ in 0..10 {
        t += Duration::from_millis(500);
        relay.handle_timeout(t, false);
        pump(&mut relay, &mut srv, t);
    }
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    assert_eq!(srv.alloc_user.as_deref(), Some("bob"));
    let f = relay.facet(t);
    assert_eq!((f.allocations, f.failures), (2, 0), "{f:?}");
    // Outside the settle window a 437 is a real failure (another allocation holds the 5-tuple,
    // e.g. the previous hub process's): backed off and counted.
    let mut relay = TurnRelay::new(opts()).unwrap();
    let mut srv = fake();
    srv.fail_allocate = Some(437);
    relay.set_credentials(creds("alice", "pw-a", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.phase(), TurnPhase::Backoff);
    assert_eq!(relay.facet(t0).failures, 1);
}

#[test]
fn the_relayed_address_is_advertised_as_a_relay_candidate_below_every_host() {
    use intercom_hub::webrtc_turn::{advertise_relayed, RELAY_CANDIDATE_PRIORITY};
    let sdp = "v=0\r\n\
a=candidate:aa 1 udp 2130706175 192.168.1.10 6992 typ host ufrag U1\r\n\
a=candidate:bb 1 udp 2130706175 203.0.113.10 50001 typ host ufrag U1\r\n\
a=end-of-candidates\r\n";
    let out = advertise_relayed(sdp, sa(RELAYED));
    assert_eq!(
        out,
        format!(
            "v=0\r\n\
a=candidate:aa 1 udp 2130706175 192.168.1.10 6992 typ host ufrag U1\r\n\
a=candidate:bb 1 udp {RELAY_CANDIDATE_PRIORITY} 203.0.113.10 50001 typ relay raddr 0.0.0.0 rport 0 ufrag U1\r\n\
a=end-of-candidates\r\n"
        )
    );
    // Type preference 0: below a host candidate whatever its local preference.
    const { assert!(RELAY_CANDIDATE_PRIORITY < 1 << 24) };
    // An SDP without that address is unchanged.
    assert_eq!(advertise_relayed(sdp, sa("203.0.113.10:50002")), sdp);
}

#[test]
fn the_same_credentials_again_change_nothing() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    relay.set_credentials(creds("alice", "pw-a", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(srv.count("allocate"), 2);
    assert_eq!(srv.count("refresh"), 0);
}

#[test]
fn a_rotation_waits_while_a_phone_is_on_the_relay_but_never_past_the_old_expiry() {
    let t0 = Instant::now();
    let mut relay = TurnRelay::new(opts()).unwrap();
    let mut srv = fake();
    let old_expiry = t0 + Duration::from_secs(3600);
    relay.set_credentials(creds("alice", "pw-a", old_expiry), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    let t = t0 + Duration::from_secs(5);
    relay.set_credentials(creds("bob", "pw-b", t + Duration::from_secs(86_400)), t);
    // Busy: the allocation stays (and keeps refreshing with alice).
    let mut t = t;
    loop {
        let next = t + Duration::from_secs(60);
        if next + TURN_ROTATE_MARGIN >= old_expiry {
            break;
        }
        t = next;
        relay.handle_timeout(t, true);
        pump(&mut relay, &mut srv, t);
        assert_eq!(srv.alloc_user.as_deref(), Some("alice"), "kept while busy");
        assert_eq!(relay.phase(), TurnPhase::Allocated);
    }
    // Inside the margin before alice's credential expires: rotated although busy.
    let t = old_expiry - TURN_ROTATE_MARGIN + Duration::from_secs(1);
    relay.handle_timeout(t, true);
    pump(&mut relay, &mut srv, t);
    assert_eq!(srv.alloc_user.as_deref(), Some("bob"));
    assert_eq!(relay.phase(), TurnPhase::Allocated);
}

#[test]
fn new_credentials_end_a_backoff_at_once() {
    let t0 = Instant::now();
    let mut relay = TurnRelay::new(opts()).unwrap();
    let mut srv = fake();
    relay.set_credentials(
        creds("mallory", "nope", t0 + Duration::from_secs(86_400)),
        t0,
    );
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(
        relay.phase(),
        TurnPhase::Backoff,
        "an unknown user gets 401 twice"
    );
    relay.set_credentials(creds("alice", "pw-a", t0 + Duration::from_secs(86_400)), t0);
    relay.handle_timeout(t0, false);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    assert_eq!(srv.alloc_user.as_deref(), Some("alice"));
}

#[test]
fn permissions_are_ipv4_bounded_and_refreshed_only_while_wanted() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let a: IpAddr = "198.51.100.1".parse().unwrap();
    let b: IpAddr = "198.51.100.2".parse().unwrap();
    let v6: IpAddr = "2001:db8::1".parse().unwrap();
    relay.set_wanted([a, b, v6], t0);
    pump(&mut relay, &mut srv, t0);
    assert!(srv.perms.contains(&a) && srv.perms.contains(&b));
    assert!(
        !srv.perms.contains(&v6),
        "an IPv4 allocation never asks for an IPv6 peer"
    );
    assert_eq!(relay.facet(t0).permissions, 2);

    // Bounded: 20 wanted, at most max_permissions (8) asked for.
    let many: Vec<IpAddr> = (1..=20u8).map(|i| IpAddr::from([198, 18, 0, i])).collect();
    relay.set_wanted(many.clone(), t0);
    pump(&mut relay, &mut srv, t0);
    let f = relay.facet(t0);
    assert_eq!(f.permissions, 8);
    assert!(f.permissions_refused >= 12, "{f:?}");

    // Only the wanted ones are refreshed (the others lapse on the server after 300 s).
    relay.set_wanted([a], t0);
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.facet(t0).permissions, 1);
    let before = srv.count("permission");
    let t = t0 + TURN_PERMISSION_REFRESH + Duration::from_secs(1);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(
        srv.count("permission"),
        before + 1,
        "one refresh, for a only"
    );
}

#[test]
fn a_refused_permission_is_counted_and_not_retried_at_once() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    srv.fail_permission = Some(403);
    let a: IpAddr = "10.1.2.3".parse().unwrap();
    relay.set_wanted([a], t0);
    pump(&mut relay, &mut srv, t0);
    let n = srv.count("permission");
    assert_eq!(n, 1);
    let t = t0 + Duration::from_secs(5);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(srv.count("permission"), n, "no tight retry loop");
    let f = relay.facet(t);
    assert_eq!(f.permissions, 0);
    assert!(f.last_error.unwrap().contains("403"));
    assert_eq!(
        relay.phase(),
        TurnPhase::Allocated,
        "a refused peer is not a lost relay"
    );
}

#[test]
fn sends_wrap_as_send_indications_then_channel_data_and_need_a_permission() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let peer = sa("198.51.100.1:61000");
    // No permission: nothing goes out.
    assert!(relay.send_to(peer, b"check-1", t0).is_err());
    assert!(relay.poll_transmit().is_none());
    relay.set_wanted([peer.ip()], t0);
    pump(&mut relay, &mut srv, t0);
    // First send: a Send indication (and a ChannelBind starts).
    relay.send_to(peer, b"check-2", t0).unwrap();
    pump(&mut relay, &mut srv, t0);
    assert_eq!(srv.relayed_out, vec![(peer, b"check-2".to_vec(), false)]);
    assert_eq!(srv.count("channel-bind"), 1);
    assert_eq!(relay.facet(t0).channels, 1);
    // Bound: ChannelData from now on.
    relay.send_to(peer, b"media-1", t0).unwrap();
    pump(&mut relay, &mut srv, t0);
    assert_eq!(srv.relayed_out[1], (peer, b"media-1".to_vec(), true));
    // Every datagram the relay ever emitted went to the TURN server (the caller sends them there).
    assert_eq!(relay.facet(t0).tx_packets, 2);
    // A peer port the offer did not name, same permitted IP (a NAT's new mapping): allowed, the
    // permission is per IP.
    let other_port = sa("198.51.100.1:62000");
    relay.send_to(other_port, b"prflx", t0).unwrap();
    pump(&mut relay, &mut srv, t0);
    assert_eq!(srv.relayed_out[2].0, other_port);
}

#[test]
fn channels_are_bounded_and_dropped_with_their_peer() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let peers: Vec<SocketAddr> = (1..=6u8)
        .map(|i| SocketAddr::from(([198, 51, 100, i], 5000)))
        .collect();
    relay.set_wanted(peers.iter().map(|p| p.ip()), t0);
    pump(&mut relay, &mut srv, t0);
    for p in &peers {
        relay.send_to(*p, b"x", t0).unwrap();
    }
    pump(&mut relay, &mut srv, t0);
    assert_eq!(relay.facet(t0).channels, 4, "max_channels");
    assert_eq!(
        srv.relayed_out.len(),
        6,
        "the rest still goes out as Send indications"
    );
    relay.set_wanted(peers[..1].iter().map(|p| p.ip()), t0);
    assert_eq!(relay.facet(t0).channels, 1);
}

#[test]
fn data_indications_and_channel_data_unwrap_to_the_peer() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let peer = sa("198.51.100.1:61000");
    relay.set_wanted([peer.ip()], t0);
    pump(&mut relay, &mut srv, t0);
    let ind = srv.deliver_from(peer, b"stun-check").unwrap();
    assert_eq!(
        relay.handle_datagram(&ind, t0),
        Some((peer, b"stun-check".to_vec()))
    );
    // Bind a channel by sending, then the server's ChannelData unwraps too.
    relay.send_to(peer, b"reply", t0).unwrap();
    pump(&mut relay, &mut srv, t0);
    let cd = srv.deliver_from(peer, b"srtp").unwrap();
    assert!(
        !rtc_stun::message::is_stun_message(&cd),
        "ChannelData, not STUN"
    );
    assert_eq!(
        relay.handle_datagram(&cd, t0),
        Some((peer, b"srtp".to_vec()))
    );
    assert_eq!(relay.facet(t0).rx_packets, 2);
    // ChannelData on a channel the relay never bound, and a Data indication from an IP it never
    // permitted, are dropped.
    assert_eq!(relay.handle_datagram(&channel_data(0x7ffe, b"x"), t0), None);
    let stranger = sa("192.0.2.99:4000");
    assert_eq!(
        relay.handle_datagram(&data_indication(stranger, b"x"), t0),
        None
    );
    // Garbage is dropped.
    assert_eq!(relay.handle_datagram(b"\x00\x01garbage", t0), None);
    assert_eq!(relay.handle_datagram(&[], t0), None);
}

#[test]
fn a_lost_allocation_needs_new_permissions_and_drops_its_channels() {
    let t0 = Instant::now();
    let (mut relay, mut srv) = allocated(t0, opts());
    let peer = sa("198.51.100.1:61000");
    relay.set_wanted([peer.ip()], t0);
    pump(&mut relay, &mut srv, t0);
    relay.send_to(peer, b"a", t0).unwrap();
    pump(&mut relay, &mut srv, t0);
    srv.forget_allocation();
    let t = t0 + Duration::from_secs(301);
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert!(
        relay.send_to(peer, b"b", t).is_err(),
        "no relay while backing off"
    );
    let t = t + TURN_BACKOFF_MIN;
    relay.handle_timeout(t, false);
    pump(&mut relay, &mut srv, t);
    assert_eq!(relay.phase(), TurnPhase::Allocated);
    // The wanted IP is permitted again on the new allocation, and a fresh channel gets bound.
    assert!(srv.perms.contains(&peer.ip()));
    assert_eq!(relay.facet(t).channels, 0);
    relay.send_to(peer, b"c", t).unwrap();
    pump(&mut relay, &mut srv, t);
    assert_eq!(
        srv.relayed_out.last().unwrap(),
        &(peer, b"c".to_vec(), false)
    );
}

#[test]
fn unavailable_credentials_show_off_with_the_reason() {
    let t0 = Instant::now();
    let mut relay = TurnRelay::new(opts()).unwrap();
    relay.set_unavailable("no TURN key file at /etc/intercom-hub/turn.toml");
    relay.handle_timeout(t0, false);
    assert!(relay.poll_transmit().is_none());
    let f = relay.facet(t0);
    assert_eq!(f.state, "off");
    assert!(f.last_error.unwrap().contains("no TURN key"));
}

#[test]
fn the_permissions_come_from_the_offers_kept_candidates() {
    use intercom_hub::webrtc_signal::{filter_offer_candidates, offer_candidate_ips};
    let sdp = "v=0\r\n\
a=candidate:1 1 udp 2122260223 10.20.30.40 50000 typ host\r\n\
a=candidate:2 1 udp 1686052607 198.51.100.7 61000 typ srflx raddr 10.20.30.40 rport 50000\r\n\
a=candidate:3 1 udp 41885439 141.101.90.1 40000 typ relay raddr 198.51.100.7 rport 61000\r\n\
a=candidate:4 1 udp 41885439 141.101.90.1 40002 typ relay raddr 198.51.100.7 rport 61000\r\n\
a=candidate:5 1 udp 2122260223 abcd.local 50001 typ host\r\n\
a=candidate:6 1 tcp 1518280447 198.51.100.8 9 typ host tcptype active\r\n";
    let own: Vec<SocketAddr> = vec![sa("192.168.1.10:6992")];
    let (kept, _) = filter_offer_candidates(sdp, &own);
    let ips: Vec<String> = offer_candidate_ips(&kept)
        .iter()
        .map(|i| i.to_string())
        .collect();
    // Each kept address once (the relay's two ports are one permission), no mDNS name, no TCP.
    assert_eq!(ips, vec!["10.20.30.40", "198.51.100.7", "141.101.90.1"]);
}

#[test]
fn the_udp_turn_url_is_picked_from_cloudflares_list() {
    let urls: Vec<String> = [
        "stun:stun.cloudflare.com:3478",
        "turn:turn.cloudflare.com:3478?transport=tcp",
        "turns:turn.cloudflare.com:5349?transport=tcp",
        "turn:turn.cloudflare.com:3478?transport=udp",
        "turn:turn.cloudflare.com:80?transport=udp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(
        turn_udp_server(&urls),
        Some(("turn.cloudflare.com".to_string(), 3478))
    );
    // No transport = UDP; the default port is 3478; a literal IPv4 host works too.
    assert_eq!(
        turn_udp_server(&["turn:192.0.2.5".to_string()]),
        Some(("192.0.2.5".to_string(), 3478))
    );
    assert_eq!(
        turn_udp_server(&["turn:192.0.2.5:3480".to_string()]),
        Some(("192.0.2.5".to_string(), 3480))
    );
    // TCP / TLS only: nothing the hub can use.
    assert_eq!(
        turn_udp_server(&["turns:turn.cloudflare.com:443?transport=tcp".to_string()]),
        None
    );
    assert_eq!(turn_udp_server(&[]), None);
}
