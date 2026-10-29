//! issue 1431 (joint event: the cameras follow the strih behind `strih.lan`): the hub sends each
//! `camN` return leg to the address `camN`'s own stream arrives from, learned per packet. The TOML
//! `host` is only the start-up address used before the first packet; a participant still exists
//! only when its leg is configured; both venue TOMLs list cam1-7 so either hub serves every camera.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;

use intercom_hub::matrix::Matrix;
use intercom_hub::return_addr::{
    LearnFilter, ReturnAddrState, ReturnAddrs, ReturnChange, ReturnSource, LEARN_CONFIRM_PACKETS,
    RETURN_PORT,
};
use intercom_hub::state::{HubState, RuntimeStats};

const PP_TOML: &str = include_str!("../../intercom.strih-pp.toml");
const SNV_TOML: &str = include_str!("../../intercom.strih-snv.toml");

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 77, 9, last))
}

fn sa(last: u8) -> SocketAddr {
    SocketAddr::new(ip(last), RETURN_PORT)
}

/// Two legs: participant 0 (`cam1`, configured 10.77.9.61) and participant 2 (`cam2`, its host did
/// not resolve); participant 1 has no leg (a program feed).
fn table() -> ReturnAddrs {
    ReturnAddrs::new(
        3,
        vec![
            (0, "cam1".to_string(), Some(sa(61))),
            (2, "cam2".to_string(), None),
        ],
    )
}

#[test]
fn before_the_first_packet_a_leg_uses_its_configured_start_up_address() {
    let t = table();
    assert_eq!(t.len(), 2);
    assert_eq!(t.currents(), vec![Some(sa(61)), None]);
    assert_eq!(
        t.states(),
        vec![
            (
                0,
                ReturnAddrState {
                    addr: Some(sa(61)),
                    source: ReturnSource::Configured
                }
            ),
            (
                2,
                ReturnAddrState {
                    addr: None,
                    source: ReturnSource::Unresolved
                }
            ),
        ]
    );
}

#[test]
fn a_packet_from_another_address_moves_the_leg_there_once() {
    let mut t = table();
    // The cambox sends from an ephemeral port and listens on 6980: only the source IP is learned.
    let change = t.learn(0, ip(62)).expect("the return address changed");
    assert_eq!(
        change,
        ReturnChange {
            leg: "cam1".into(),
            old: Some(sa(61)),
            new: sa(62)
        }
    );
    assert_eq!(
        change.log_line(),
        "leg cam1 return address 10.77.9.61:6980 -> 10.77.9.62:6980"
    );
    assert_eq!(t.current(0), Some(sa(62)));
    assert_eq!(t.states()[0].1.source, ReturnSource::Learned);
    // The same source again is no change (logged once per change).
    assert_eq!(t.learn(0, ip(62)), None);
    // Back to another address is a new change.
    let back = t.learn(0, ip(61)).expect("moved again");
    assert_eq!((back.old, back.new), (Some(sa(62)), sa(61)));
}

#[test]
fn a_first_packet_from_the_configured_address_changes_nothing_but_the_source() {
    let mut t = table();
    assert_eq!(t.learn(0, ip(61)), None, "the address did not change");
    assert_eq!(t.current(0), Some(sa(61)));
    assert_eq!(t.states()[0].1.source, ReturnSource::Learned);
}

#[test]
fn an_unresolved_start_up_host_is_filled_by_the_first_packet() {
    let mut t = table();
    let c = t.learn(2, ip(65)).expect("unresolved -> learned");
    assert_eq!((c.old, c.new), (None, sa(65)));
    assert_eq!(
        c.log_line(),
        "leg cam2 return address unresolved -> 10.77.9.65:6980"
    );
    assert_eq!(t.currents(), vec![Some(sa(61)), Some(sa(65))]);
}

#[test]
fn a_participant_without_a_configured_leg_never_gets_one() {
    let mut t = table();
    assert_eq!(t.learn(1, ip(99)), None);
    assert_eq!(t.learn(7, ip(99)), None, "an unknown id is ignored");
    assert_eq!(t.len(), 2);
    assert!(t.states().iter().all(|(id, _)| *id != 1));
}

#[test]
fn a_dns_refresh_never_overrides_a_learned_address() {
    let mut t = table();
    t.learn(0, ip(62)).unwrap();
    assert_eq!(t.set_configured(0, sa(70)), None, "the learned one stays");
    assert_eq!(t.current(0), Some(sa(62)));
    // Before any packet a refresh moves the start-up address and says so.
    let c = t.set_configured(1, sa(71)).expect("start-up moved");
    assert_eq!((c.leg.as_str(), c.old, c.new), ("cam2", None, sa(71)));
    assert_eq!(t.current(1), Some(sa(71)));
    assert_eq!(t.set_configured(1, sa(71)), None, "the same address again");
}

/// Feed `n` packets of one source; true when the filter asked to learn on any of them.
fn feed(f: &mut LearnFilter, id: usize, from: IpAddr, n: u32) -> u32 {
    (0..n).filter(|_| f.is_new(id, from)).count() as u32
}

#[test]
fn the_learn_filter_locks_the_table_only_on_a_new_steady_source() {
    let mut f = LearnFilter::new(3);
    // A new source is taken only after LEARN_CONFIRM_PACKETS packets in a row from it.
    assert_eq!(feed(&mut f, 0, ip(61), LEARN_CONFIRM_PACKETS - 1), 0);
    assert!(f.is_new(0, ip(61)), "confirmed on its Nth packet");
    assert_eq!(
        feed(&mut f, 0, ip(61), 1000),
        0,
        "the same source again is never new"
    );
    assert_eq!(
        feed(&mut f, 0, ip(62), LEARN_CONFIRM_PACKETS),
        1,
        "a moved camera"
    );
    assert_eq!(
        feed(&mut f, 2, ip(61), LEARN_CONFIRM_PACKETS),
        1,
        "per participant"
    );
    assert_eq!(
        feed(&mut f, 9, ip(61), 100),
        0,
        "an id outside the matrix is never new"
    );
}

/// Review round 1: two senders using one stream name (a stray tool, a duplicated box) must not
/// flip the leg, lock the table or log on every packet.
#[test]
fn two_alternating_sources_never_flip_the_leg() {
    let mut f = LearnFilter::new(1);
    let mut asks = 0;
    for _ in 0..10_000 {
        asks += u32::from(f.is_new(0, ip(61)));
        asks += u32::from(f.is_new(0, ip(62)));
    }
    assert_eq!(asks, 0, "no source is ever steady");
    // A burst from one of them (a few packets out of order) still does not move the leg.
    assert_eq!(feed(&mut f, 0, ip(61), LEARN_CONFIRM_PACKETS - 1), 0);
    assert!(!f.is_new(0, ip(62)));
}

/// Review round 1: a dual-stack bind reports an IPv4 sender as `::ffff:a.b.c.d`; the senders are
/// IPv4 sockets, so the leg learns the plain IPv4 address, and a real IPv6 source is never taken.
#[test]
fn an_ipv4_mapped_source_is_learned_as_ipv4_and_ipv6_is_ignored() {
    let mut t = table();
    let mapped = IpAddr::V6(Ipv4Addr::new(10, 77, 9, 62).to_ipv6_mapped());
    let c = t.learn(0, mapped).expect("moved to the mapped camera");
    assert_eq!(c.new, sa(62));
    assert_eq!(t.learn(2, IpAddr::V6(Ipv6Addr::LOCALHOST)), None);
    assert_eq!(
        t.current(1),
        None,
        "an IPv6 source never becomes a return address"
    );
    let mut f = LearnFilter::new(1);
    assert_eq!(feed(&mut f, 0, mapped, LEARN_CONFIRM_PACKETS - 1), 0);
    assert!(f.is_new(0, ip(62)), "mapped and plain are one source");
}

#[test]
fn api_state_shows_each_legs_return_address() {
    let m = Matrix::from_toml(PP_TOML).unwrap();
    let mut t = ReturnAddrs::from_matrix(&m, |h| {
        h.parse().ok().map(|i| SocketAddr::new(i, RETURN_PORT))
    });
    let cam3 = m.id_of("cam3").expect("cam3 on the PP hub");
    t.learn(cam3, ip(163)).unwrap();
    let stats = vec![RuntimeStats::default(); m.participants.len()];
    let mut hs = HubState::snapshot(&m, "test", &stats);
    hs.attach_return_addrs(t.states());
    let v = serde_json::to_value(&hs).unwrap();
    let by = |name: &str| {
        let id = m.id_of(name).unwrap();
        v["participants"][id]["return_addr"].clone()
    };
    assert_eq!(
        by("cam3"),
        serde_json::json!({"addr": "10.77.9.163:6980", "source": "learned"})
    );
    let cam2 = m.id_of("cam2").unwrap();
    let mut no_host = ReturnAddrs::new(m.participants.len(), vec![(cam2, "cam2".into(), None)]);
    let mut hs2 = HubState::snapshot(&m, "test", &stats);
    hs2.attach_return_addrs(no_host.states());
    let v2 = serde_json::to_value(&hs2).unwrap();
    assert_eq!(
        v2["participants"][cam2]["return_addr"],
        serde_json::json!({"addr": null, "source": "unresolved"})
    );
    assert!(no_host.learn(cam2, ip(62)).is_some());
    assert_eq!(
        by("cam1"),
        serde_json::json!({"addr": "10.77.9.61:6980", "source": "configured"})
    );
    assert!(by("lv1").is_null(), "no leg, no field");
    assert!(by("cutters").is_null());
}

/// The PP hub serves every camera: cam1-7 at their literal 10.77.9.6N (the PP router has no
/// `camN.lan`), the SNV hub keeps `camN.lan`.
#[test]
fn both_venue_hubs_list_cam1_to_cam7() {
    for (toml, host) in [
        (
            PP_TOML,
            (|n: u32| format!("10.77.9.6{n}")) as fn(u32) -> String,
        ),
        (SNV_TOML, |n: u32| format!("cam{n}.lan")),
    ] {
        let m = Matrix::from_toml(toml).unwrap();
        let outs = m.vban_outputs();
        let mut legs: Vec<(String, String, String)> = outs
            .iter()
            .map(|(id, stream, h)| (m.participants[*id].name.clone(), stream.clone(), h.clone()))
            .collect();
        legs.sort();
        let want: Vec<(String, String, String)> = (1..=7)
            .map(|n| (format!("cam{n}"), format!("cam{n}"), host(n)))
            .collect();
        assert_eq!(legs, want);
        for n in 1..=7 {
            let id = m.id_of(&format!("cam{n}")).unwrap();
            assert_eq!(
                m.participants[id].in_stream.as_deref(),
                Some(format!("cam{n}").as_str()),
                "each leg learns from its own stream"
            );
        }
    }
}

/// The PP hub's camera mesh: every camera hears every other camera, the cutters and the phones on
/// both ears, never itself (the N-1 invariant also refused at load).
#[test]
fn the_pp_hub_meshes_all_seven_cameras() {
    let m = Matrix::from_toml(PP_TOML).unwrap();
    let cams: Vec<usize> = (1..=7)
        .map(|n| m.id_of(&format!("cam{n}")).unwrap())
        .collect();
    let has = |src: usize, in_ch: usize, dst: usize, out_ch: usize| {
        m.points
            .iter()
            .any(|p| (p.src, p.in_ch, p.dst, p.out_ch) == (src, in_ch, dst, out_ch))
    };
    let cutters = m.id_of("cutters").unwrap();
    let phones = m.id_of("phones").unwrap();
    for &a in &cams {
        for ch in 1..=2 {
            for &b in &cams {
                if a != b {
                    assert!(has(a, ch, b, ch), "{a} ch{ch} -> {b}");
                }
            }
            assert!(has(a, ch, cutters, ch) && has(a, ch, phones, ch));
            assert!(has(phones, ch, a, ch));
            assert!(has(cutters, 1, a, ch) && has(cutters, 2, a, ch));
        }
    }
    assert!(m.points.iter().all(|p| p.src != p.dst));
}

/// The daemon wiring: the receive task learns from the packet's source through the filter, the
/// block loop sends to the table's current addresses, the DNS refresh only moves the start-up
/// address, and `/api/state` carries the table.
#[test]
fn main_learns_in_the_receive_task_and_sends_from_the_table() {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let src = std::fs::read_to_string(&p).expect("read main.rs");
    let body = |name: &str| -> &str {
        let from = &src[src.find(name).unwrap()..];
        &from[..from[1..].find("\nfn ").map_or(from.len(), |i| i + 1)]
    };
    let recv = body("fn spawn_vban_receive(");
    let audio = recv
        .find("RoutedPacket::Audio {")
        .expect("the decoded-audio arm");
    let filter = recv
        .find("learn_filter.is_new(id, from.ip())")
        .expect("the filter");
    let converter = recv
        .find("converters.get_mut(id)")
        .expect("the rate converter");
    assert!(
        audio < filter && filter < converter,
        "learning sits in the decoded-audio arm, before the converter"
    );
    assert!(recv[filter..converter].contains(".learn(id, from.ip())"));
    assert!(recv[filter..converter].contains("change.log_line()"));
    assert!(
        !recv[..audio].contains(".learn("),
        "never before the packet decoded"
    );
    let lp = body("fn run_block_loop(");
    assert!(lp.contains(".currents()"));
    assert!(lp.contains("attach_return_addrs("));
    assert!(
        !lp.contains("unwrap_or_default"),
        "a poisoned table lock never silences every leg"
    );
    let refresh = body("fn spawn_output_resolve_refresh(");
    assert!(refresh.contains(".set_configured("));
    assert!(
        !src.contains("out_addrs"),
        "the one table replaces the address vector"
    );
}
