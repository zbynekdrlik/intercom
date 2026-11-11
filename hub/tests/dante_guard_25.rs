//! intercom#25 (dstack two-networks.md §4, option B): the hub's HTTP, VBAN and Janus RTP sockets
//! keep listening on all addresses and drop at once whatever arrives on the Dante leg; the NDI
//! picture never connects a source at a Dante address; a bind inside the Dante leg is refused.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use axum::routing::get;
use axum::Router;
use intercom_hub::dante_guard::{
    build_nets, check_bind, parse_subnets, resolve_iface, DanteGuard, DanteNets, IfaceInfo,
    Ipv4Net, PacketInfo,
};
use intercom_hub::ndi_video::source::ndi_url_ip;

fn net(s: &str) -> Ipv4Net {
    s.parse().unwrap()
}

fn v4(a: u8, b: u8, c: u8, d: u8) -> Ipv4Addr {
    Ipv4Addr::new(a, b, c, d)
}

fn lo_index() -> u32 {
    let name = std::ffi::CString::new("lo").unwrap();
    // SAFETY: a valid NUL-terminated name.
    let i = unsafe { libc::if_nametoindex(name.as_ptr()) };
    assert_ne!(i, 0, "the test box has a loopback interface");
    i
}

// --- the subnet ---------------------------------------------------------------------------------

#[test]
fn a_cidr_parses_and_contains_only_its_own_addresses() {
    let n = net("10.20.30.40/24");
    assert_eq!(n.network(), v4(10, 20, 30, 0), "host bits are cleared");
    assert_eq!(n.prefix(), 24);
    assert!(n.contains(v4(10, 20, 30, 1)));
    assert!(n.contains(v4(10, 20, 30, 255)));
    assert!(!n.contains(v4(10, 20, 31, 1)));
    assert_eq!(net("10.20.30.40").prefix(), 32, "a bare address is a /32");
    assert!(net("0.0.0.0/0").contains(v4(8, 8, 8, 8)));
    assert!("10.0.0.0/33".parse::<Ipv4Net>().is_err());
    assert!("10.0.0/8".parse::<Ipv4Net>().is_err());
    assert!("fe80::1/64".parse::<Ipv4Net>().is_err());
}

#[test]
fn dante_subnet_takes_a_comma_list_and_refuses_garbage() {
    assert_eq!(parse_subnets("").unwrap(), vec![]);
    assert_eq!(
        parse_subnets(" 10.70.0.0/16 , 192.168.70.0/24").unwrap(),
        vec![net("10.70.0.0/16"), net("192.168.70.0/24")]
    );
    let e = parse_subnets("10.70.0.0/16,nope").unwrap_err();
    assert!(format!("{e:#}").contains("DANTE_SUBNET"), "{e:#}");
}

// --- the decisions ------------------------------------------------------------------------------

fn dante() -> DanteNets {
    DanteNets {
        ifindex: Some(7),
        iface_addr: Some(v4(169, 254, 9, 9)),
        nets: vec![net("10.70.0.0/16")],
    }
}

#[test]
fn a_connection_is_refused_only_on_a_dante_local_address() {
    let d = dante();
    let at = |ip: IpAddr| SocketAddr::new(ip, 8790);
    assert!(d.refuses_connection(at(IpAddr::V4(v4(10, 70, 1, 2)))), "inside DANTE_SUBNET");
    assert!(d.refuses_connection(at(IpAddr::V4(v4(169, 254, 9, 9)))), "the dlink iface's address");
    assert!(
        d.refuses_connection(at(IpAddr::V6(v4(10, 70, 1, 2).to_ipv6_mapped()))),
        "an IPv4-mapped Dante address on a dual-stack socket"
    );
    assert!(!d.refuses_connection(at(IpAddr::V4(v4(10, 77, 9, 202)))), "the LAN");
    assert!(!d.refuses_connection(at(IpAddr::V4(Ipv4Addr::LOCALHOST))), "loopback: the box's nginx");
    assert!(!d.refuses_connection(at(IpAddr::V6(Ipv6Addr::LOCALHOST))));
}

#[test]
fn a_datagram_is_refused_on_the_dlink_iface_or_to_a_dante_address() {
    let d = dante();
    let lan = v4(10, 77, 9, 202);
    assert!(d.refuses_datagram(Some(PacketInfo { ifindex: 7, dst: lan })), "came in on the dlink iface");
    assert!(
        d.refuses_datagram(Some(PacketInfo { ifindex: 2, dst: v4(10, 70, 0, 5) })),
        "sent to a DANTE_SUBNET address"
    );
    assert!(!d.refuses_datagram(Some(PacketInfo { ifindex: 2, dst: lan })), "LAN in, LAN address");
    assert!(!d.refuses_datagram(None), "no packet info: nothing can be told");
    assert!(!DanteNets::default().refuses_datagram(Some(PacketInfo { ifindex: 7, dst: lan })));
}

#[test]
fn the_view_adds_the_dlink_iface_and_its_subnet_to_the_configured_ones() {
    let resolve = |name: &str| {
        (name == "enp7s0.70").then_some(IfaceInfo {
            ifindex: 9,
            addr: Some((v4(10, 70, 3, 4), net("10.70.0.0/16"))),
        })
    };
    let only_iface = build_nets(Some("enp7s0.70"), &[], resolve);
    assert_eq!(only_iface.ifindex, Some(9));
    assert_eq!(only_iface.iface_addr, Some(v4(10, 70, 3, 4)));
    assert_eq!(only_iface.nets, vec![net("10.70.0.0/16")]);
    let both = build_nets(Some("enp7s0.70"), &[net("10.70.0.0/16")], resolve);
    assert_eq!(both.nets, vec![net("10.70.0.0/16")], "the same subnet is not listed twice");
    let missing = build_nets(Some("enp9s9.70"), &[net("192.168.70.0/24")], resolve);
    assert_eq!(missing.ifindex, None, "an iface dlink has not brought up yet");
    assert_eq!(missing.nets, vec![net("192.168.70.0/24")], "DANTE_SUBNET still holds");
    assert_eq!(build_nets(None, &[], resolve), DanteNets::default());
}

#[test]
fn the_loopback_iface_resolves_through_the_os() {
    let lo = resolve_iface("lo").expect("lo resolves");
    assert_eq!(lo.ifindex, lo_index());
    assert_eq!(lo.addr, Some((Ipv4Addr::LOCALHOST, net("127.0.0.0/8"))));
    assert_eq!(resolve_iface("no-such-iface0"), None);
}

#[test]
fn a_guard_with_neither_iface_nor_subnet_filters_nothing() {
    let g = DanteGuard::new(None, vec![]);
    assert!(!g.active());
    let from: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    assert!(!g.check_connection("127.0.0.1:8790".parse().unwrap(), from));
    assert!(g.refusals().is_empty());
}

#[test]
fn a_bind_inside_the_dante_leg_is_refused_at_start() {
    let g = DanteGuard::new(None, vec![net("10.70.0.0/16")]);
    let e = check_bind(&g, "janus rtp_bind", "10.70.0.9:6990".parse().unwrap()).unwrap_err();
    assert!(e.to_string().contains("Dante address"), "{e}");
    check_bind(&g, "vban_bind", "0.0.0.0:6980".parse().unwrap()).unwrap();
    check_bind(&g, "bind", "10.77.9.202:8790".parse().unwrap()).unwrap();
}

// --- UDP: the real IP_PKTINFO path --------------------------------------------------------------

#[tokio::test]
async fn a_datagram_on_the_dlink_iface_is_dropped_and_counted_per_port() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    intercom_hub::pktinfo::enable(&socket).unwrap();
    let port = socket.local_addr().unwrap().port();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sender.send_to(b"vban", ("127.0.0.1", port)).unwrap();
    let mut buf = [0u8; 64];
    let (n, from, info) = tokio::time::timeout(
        Duration::from_secs(2),
        intercom_hub::pktinfo::recv_from_with_info(&socket, &mut buf),
    )
    .await
    .expect("a datagram within 2 s")
    .unwrap();
    assert_eq!(&buf[..n], b"vban");
    assert_eq!(from, sender.local_addr().unwrap());
    let info = info.expect("IP_PKTINFO attached");
    assert_eq!(info, PacketInfo { ifindex: lo_index(), dst: Ipv4Addr::LOCALHOST });

    // "lo" as the dlink iface: the same datagram is the Dante leg's.
    let g = DanteGuard::new(Some("lo".to_string()), vec![]);
    assert!(g.active());
    assert!(g.check_datagram(port, Some(info), from));
    assert!(g.check_datagram(port, Some(info), from));
    assert_eq!(g.refusals().get(&port), Some(&2), "counted per port");
    // A LAN-shaped datagram on another iface is kept.
    let lan = PacketInfo { ifindex: lo_index() + 1000, dst: v4(10, 77, 9, 202) };
    assert!(!g.check_datagram(port, Some(lan), from));
    assert_eq!(g.refusals().get(&port), Some(&2));
}

// --- TCP: the real accept loop ------------------------------------------------------------------

async fn start(guard: std::sync::Arc<DanteGuard>) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new().route("/", get(|| async { "hub-ok" }));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(intercom_hub::http_serve::serve(listener, router, guard, async move {
        let _ = stopped.await;
    }));
    (addr, stop)
}

fn get_root(addr: SocketAddr) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    // A refused connection may already be closed when the request goes out.
    let _ = s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_on_a_dante_address_is_closed_without_a_response() {
    let guard = DanteGuard::new(None, vec![net("127.0.0.0/8")]);
    let (addr, _stop) = start(guard.clone()).await;
    let out = tokio::task::spawn_blocking(move || get_root(addr)).await.unwrap();
    assert_eq!(out, "", "closed before any byte of a response");
    assert_eq!(guard.refusals().get(&addr.port()), Some(&1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_on_a_lan_address_is_served() {
    let guard = DanteGuard::new(None, vec![net("10.70.0.0/16")]);
    let (addr, stop) = start(guard.clone()).await;
    let out = tokio::task::spawn_blocking(move || get_root(addr)).await.unwrap();
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.ends_with("hub-ok"), "{out}");
    assert!(guard.refusals().is_empty());
    stop.send(()).unwrap();
}

// --- NDI discovery ------------------------------------------------------------------------------

#[test]
fn an_ndi_source_url_gives_its_address() {
    assert_eq!(ndi_url_ip("10.77.9.202:5961"), Some(IpAddr::V4(v4(10, 77, 9, 202))));
    assert_eq!(ndi_url_ip("10.70.0.5:5961"), Some(IpAddr::V4(v4(10, 70, 0, 5))));
    assert_eq!(ndi_url_ip("[fe80::1]:5961"), Some("fe80::1".parse().unwrap()));
    assert_eq!(ndi_url_ip(""), None);
    assert_eq!(ndi_url_ip("garbage"), None);
}

// --- the daemon's wiring ------------------------------------------------------------------------

fn src(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

#[test]
fn the_daemon_builds_one_guard_from_the_dlink_iface_and_dante_subnet() {
    let main = src("src/main.rs");
    assert!(main.contains("DanteGuard::from_env("), "main builds the guard from DANTE_SUBNET");
    assert!(main.contains(".spawn_refresher()"), "the dlink iface is followed");
    for bind in ["\"bind\"", "\"vban_bind\"", "\"janus rtp_bind\""] {
        assert!(main.contains(bind), "check_bind covers {bind}");
    }
}

#[test]
fn every_socket_asks_the_guard() {
    let main = src("src/main.rs");
    assert!(main.contains("http_serve::serve("), "HTTP goes through the refusing accept loop");
    assert!(!main.contains("axum::serve("), "never the plain axum::serve");
    assert!(main.contains("pktinfo::enable(&vban_socket)"));
    assert!(main.contains("recv_from_with_info(&vban_socket"), "VBAN receives with IP_PKTINFO");
    assert!(main.contains("check_datagram(vban_port"), "VBAN drops a Dante datagram");
    let rtp = src("src/janus_rtp.rs");
    assert!(rtp.contains("pktinfo::enable("), "RTP asks for IP_PKTINFO");
    assert!(rtp.contains("recv_from_with_info(&socket"), "RTP receives with IP_PKTINFO");
    assert!(rtp.contains("check_datagram("), "RTP drops a Dante datagram");
    let ndi = src("src/ndi_video/ndi_source.rs");
    assert!(ndi.contains("is_dante("), "NDI discovery skips a Dante source");
}

#[test]
fn the_refusals_are_on_api_state() {
    let state = src("src/state.rs");
    assert!(state.contains("pub dante_refusals: BTreeMap<u16, u64>"));
    let main = src("src/main.rs");
    assert!(main.contains("dante_refusals = "), "the daemon fills it");
}
