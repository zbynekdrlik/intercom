//! The in-hub WebRTC phone leg's pure parts (intercom#42): the `[webrtc]` table and the `webrtc`
//! adapter in `Matrix::from_toml`, the phone ids, the signalling limits, the slot lifecycle and the
//! transport seam the page reads.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use intercom_hub::matrix::{Matrix, ADAPTER_WEBRTC};
use intercom_hub::webrtc_http::transport_json;
use intercom_hub::webrtc_signal::{
    clean_display, client_key, filter_offer_candidates, is_json_content_type, is_lan_candidate,
    parse_display_body, parse_offer_body, pending_cap, PhoneId, RateLimiter, RefusalLog,
    SignalError, SlotTable, WebrtcConfig, DEFAULT_DISPLAY, DEFAULT_MAX_PHONES, DEFAULT_UDP_BIND,
    MAX_DISPLAY_CHARS, MAX_OFFER_CANDIDATES, MAX_SDP_BYTES, MAX_SIGNAL_BODY_BYTES, OFFER_BURST,
    OFFER_PER_S,
};

fn hub_toml(phones_role: &str, phones_adapter: &str, ch: (usize, usize), extra: &str) -> String {
    format!(
        r#"
[hub]
bind = "0.0.0.0:8790"
vban_bind = "0.0.0.0:6980"
sample_rate = 48000
block_frames = 256
{extra}

[[participant]]
name = "cam1"
role = "cambox"
adapter = "vban"
host = "cam1.lan"
in_stream = "cam1"
out_stream = "cam1"
in_channels = 2
out_channels = 2

[[participant]]
name = "phones"
role = "{phones_role}"
adapter = "{phones_adapter}"
in_channels = {}
out_channels = {}

[[point]]
src = "cam1"
in_ch = 1
dst = "phones"
out_ch = 1

[[point]]
src = "phones"
in_ch = 1
dst = "cam1"
out_ch = 1
"#,
        ch.0, ch.1
    )
}

fn load(toml: &str) -> Result<Matrix, String> {
    Matrix::from_toml(toml).map_err(|e| format!("{e:#}"))
}

#[test]
fn the_webrtc_adapter_loads_on_the_phones_role_with_its_table() {
    let m = load(&hub_toml("phones", "webrtc", (2, 2), "[webrtc]\n")).expect("loads");
    let pid = m.webrtc_participant().expect("the webrtc participant");
    assert_eq!(m.participants[pid].adapter, ADAPTER_WEBRTC);
    assert_eq!(m.janus_participant(), None);
    let w = m.webrtc.expect("the table");
    assert_eq!(w.udp_bind, DEFAULT_UDP_BIND);
    assert_eq!(w.udp_bind, "0.0.0.0:6992");
    assert_eq!(w.max_phones, DEFAULT_MAX_PHONES);
    assert_eq!(w.max_phones, 16);
    assert!(w.host_ips.is_empty());
}

#[test]
fn the_webrtc_adapter_is_refused_off_the_phones_role_and_with_other_channels() {
    let e = load(&hub_toml("cutters", "webrtc", (2, 2), "[webrtc]\n")).unwrap_err();
    assert!(e.contains("only allowed on role 'phones'"), "{e}");
    let e = load(&hub_toml("phones", "webrtc", (1, 2), "[webrtc]\n")).unwrap_err();
    assert!(e.contains("2 in / 2 out"), "{e}");
}

#[test]
fn the_webrtc_adapter_needs_its_table_and_the_table_needs_the_adapter() {
    let e = load(&hub_toml("phones", "webrtc", (2, 2), "")).unwrap_err();
    assert!(e.contains("needs a [webrtc] table"), "{e}");
    let e = load(&hub_toml("phones", "none", (2, 2), "[webrtc]\n")).unwrap_err();
    assert!(e.contains("no participant has adapter 'webrtc'"), "{e}");
}

#[test]
fn the_phones_leg_is_janus_or_webrtc_never_both() {
    let mut t = hub_toml("phones", "webrtc", (2, 2), "[webrtc]\n");
    t.push_str(
        r#"
[[participant]]
name = "phones2"
role = "phones"
adapter = "janus"
in_channels = 2
out_channels = 2
"#,
    );
    let e = load(&t).unwrap_err();
    assert!(e.contains("either 'janus' or 'webrtc'"), "{e}");
    let mut t = hub_toml("phones", "webrtc", (2, 2), "[webrtc]\n");
    t.push_str(
        r#"
[[participant]]
name = "phones2"
role = "phones"
adapter = "webrtc"
in_channels = 2
out_channels = 2
"#,
    );
    let e = load(&t).unwrap_err();
    assert!(e.contains("at most one 'webrtc'"), "{e}");
}

#[test]
fn the_webrtc_table_is_validated_at_load() {
    for (extra, needle) in [
        ("[webrtc]\nudp_bind = \"nonsense\"\n", "webrtc.udp_bind"),
        ("[webrtc]\nudp_bind = \"0.0.0.0:0\"\n", "fixed port"),
        ("[webrtc]\nmax_phones = 0\n", "max_phones must be 1..=64"),
        ("[webrtc]\nmax_phones = 65\n", "max_phones must be 1..=64"),
        (
            "[webrtc]\nhost_ips = [\"0.0.0.0\"]\n",
            "not an address a phone can send to",
        ),
        // The TURN relay is phase 3: the key is reserved and refused until then.
        (
            "[webrtc]\nturn = { url = \"turn:x\" }\n",
            "reserved for the hub-side TURN relay",
        ),
        ("[webrtc]\nunknown_key = 1\n", "unknown field"),
    ] {
        let e = load(&hub_toml("phones", "webrtc", (2, 2), extra)).unwrap_err();
        assert!(e.contains(needle), "{extra}: {e}");
    }
    let m = load(&hub_toml(
        "phones",
        "webrtc",
        (2, 2),
        "[webrtc]\nudp_bind = \"10.0.0.5:7000\"\nmax_phones = 64\nhost_ips = [\"10.0.0.5\"]\n",
    ))
    .expect("a full table loads");
    let w = m.webrtc.unwrap();
    assert_eq!(w.validate().unwrap(), "10.0.0.5:7000".parse().unwrap());
    assert_eq!(w.host_ips, vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))]);
}

#[test]
fn both_deployed_strih_tomls_stay_on_janus_in_this_phase() {
    for (name, text) in [
        ("strih-snv", include_str!("../../intercom.strih-snv.toml")),
        ("strih-pp", include_str!("../../intercom.strih-pp.toml")),
    ] {
        let m = load(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            m.janus_participant().is_some(),
            "{name} keeps the janus phones leg"
        );
        assert_eq!(
            m.webrtc_participant(),
            None,
            "{name}: the switch is phase 4"
        );
        assert!(m.webrtc.is_none(), "{name}");
    }
}

#[test]
fn phone_ids_are_128_random_bits_in_32_hex() {
    let a = PhoneId::random().unwrap();
    let b = PhoneId::random().unwrap();
    assert_ne!(a, b);
    let s = a.to_string();
    assert_eq!(s.len(), 32);
    assert!(s
        .bytes()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    assert_eq!(PhoneId::parse(&s), Some(a));
    assert_eq!(a.short(), s[..8]);
    let fixed = PhoneId::from_bytes([0xab; 16]);
    assert_eq!(fixed.to_string(), "ab".repeat(16));
    for bad in [
        "",
        "ab",
        &"AB".repeat(16),
        &"zz".repeat(16),
        &"ab".repeat(17),
        "../etc/passwd",
    ] {
        assert_eq!(PhoneId::parse(bad), None, "{bad}");
    }
}

fn offer_body(sdp: &str, display: Option<&str>) -> Vec<u8> {
    let mut v = serde_json::json!({ "sdp": sdp });
    if let Some(d) = display {
        v["display"] = serde_json::json!(d);
    }
    serde_json::to_vec(&v).unwrap()
}

#[test]
fn an_offer_body_is_size_checked_before_and_after_parsing() {
    let ok = parse_offer_body(&offer_body(
        "v=0\r\no=- 1 1 IN IP4 0.0.0.0\r\n",
        Some("Kamera 1"),
    ))
    .unwrap();
    assert_eq!(ok.display, "Kamera 1");
    assert!(ok.sdp.starts_with("v=0"));
    // An SDP of exactly the limit passes the size check; one byte more is 413.
    let at_limit = format!("v=0{}", "x".repeat(MAX_SDP_BYTES - 3));
    assert!(parse_offer_body(&offer_body(&at_limit, None)).is_ok());
    let over = format!("v=0{}", "x".repeat(MAX_SDP_BYTES - 2));
    assert_eq!(
        parse_offer_body(&offer_body(&over, None)),
        Err(SignalError::TooLarge)
    );
    // A body over the transport limit is refused before any JSON is parsed.
    let huge = vec![b' '; MAX_SIGNAL_BODY_BYTES + 1];
    assert_eq!(parse_offer_body(&huge), Err(SignalError::TooLarge));
    assert!(matches!(
        parse_offer_body(b"{not json"),
        Err(SignalError::BadRequest(_))
    ));
    assert!(matches!(
        parse_offer_body(b"{\"display\":\"x\"}"),
        Err(SignalError::BadRequest(_))
    ));
    assert!(matches!(
        parse_offer_body(&offer_body("hello", None)),
        Err(SignalError::BadOffer(_))
    ));
    // No display: the default name.
    assert_eq!(
        parse_offer_body(&offer_body("v=0", None)).unwrap().display,
        DEFAULT_DISPLAY
    );
}

#[test]
fn signal_errors_map_to_their_http_status() {
    assert_eq!(SignalError::BadRequest("x".into()).status(), 400);
    assert_eq!(SignalError::BadOffer("x".into()).status(), 400);
    assert_eq!(SignalError::TooLarge.status(), 413);
    assert_eq!(SignalError::Full.status(), 503);
    assert_eq!(SignalError::Unavailable.status(), 503);
    assert_eq!(SignalError::UnknownPhone.status(), 404);
    assert!(SignalError::TooLarge.to_string().contains("16384"));
}

#[test]
fn display_names_are_cleaned_never_refused() {
    assert_eq!(clean_display("  Kamera   4 \n"), "Kamera 4");
    assert_eq!(clean_display("a\u{0}b\u{7}c"), "abc");
    assert_eq!(clean_display(""), DEFAULT_DISPLAY);
    assert_eq!(clean_display(" \t "), DEFAULT_DISPLAY);
    let long = "ž".repeat(200);
    assert_eq!(clean_display(&long).chars().count(), MAX_DISPLAY_CHARS);
    assert_eq!(
        parse_display_body(" {\"display\":\" Réžia \"}".as_bytes()).unwrap(),
        "Réžia"
    );
    assert!(matches!(
        parse_display_body(b"{}"),
        Err(SignalError::BadRequest(_))
    ));
    assert_eq!(
        parse_display_body(&vec![b'x'; MAX_SIGNAL_BODY_BYTES + 1]),
        Err(SignalError::TooLarge)
    );
}

#[test]
fn slots_are_allocated_lowest_first_up_to_the_cap_and_freed_by_generation() {
    let mut t = SlotTable::new(3);
    assert_eq!(t.capacity(), 3);
    let a = t.allocate().unwrap();
    let b = t.allocate().unwrap();
    let c = t.allocate().unwrap();
    assert_eq!((a.0, b.0, c.0), (0, 1, 2));
    assert!(
        a.1 < b.1 && b.1 < c.1,
        "every allocation gets a new generation"
    );
    assert_eq!(t.in_use(), 3);
    assert_eq!(t.allocate(), None, "full: the 503 case");
    // A stale free (an old generation) changes nothing.
    assert!(!t.free(1, b.1 + 100));
    assert_eq!(t.generation(1), Some(b.1));
    assert!(t.free(1, b.1));
    assert!(!t.free(1, b.1), "freed once only");
    assert_eq!(t.generation(1), None);
    let d = t.allocate().unwrap();
    assert_eq!(d.0, 1, "the freed slot is reused");
    assert!(d.1 > c.1, "with a fresh generation");
    assert!(!t.free(7, d.1), "out of range");
    assert_eq!(t.generation(7), None);
}

#[test]
fn the_page_reads_its_transport_from_api_phone() {
    assert_eq!(
        transport_json(true),
        serde_json::json!({"transport": "webrtc"})
    );
    assert_eq!(
        transport_json(false),
        serde_json::json!({"transport": "janus"})
    );
}

// intercom#42 review 🟡5: the answer goes to anonymous POSTers, so the default host candidates are
// the box's LAN addresses only.
#[test]
fn host_candidates_come_from_the_config_then_the_bind_then_the_lan() {
    let bind_any: SocketAddr = "0.0.0.0:6992".parse().unwrap();
    let ip = |s: &str| -> IpAddr { s.parse().unwrap() };
    let box_addrs: Vec<(String, IpAddr)> = [
        ("lo", "127.0.0.1"),
        ("enp3s0", "10.77.9.202"),
        ("enp3s0", "10.77.9.202"),
        ("enp5s0", "192.168.50.2"), // the Dante NIC (the guard says so)
        ("enp6s0", "169.254.10.2"), // link-local
        ("docker0", "172.17.0.1"),
        ("br-1a2b3c", "172.18.0.1"),
        ("veth12ab", "172.19.0.1"),
        ("tailscale0", "100.104.8.125"),
        ("wlp2s0", "100.64.3.4"), // CGNAT range on a real NIC: still not the LAN
        ("wlp2s0", "84.16.1.2"),  // a public address
        ("enp7s0", "172.16.5.5"),
    ]
    .iter()
    .map(|(n, a)| (n.to_string(), ip(a)))
    .collect();
    let cfg = WebrtcConfig::default();
    let got = cfg.candidate_ips(bind_any, &box_addrs, |a| a == ip("192.168.50.2"));
    assert_eq!(got, vec![ip("10.77.9.202"), ip("172.16.5.5")]);
    // A specific bind is the only candidate.
    let got = cfg.candidate_ips("127.0.0.1:6992".parse().unwrap(), &box_addrs, |_| false);
    assert_eq!(got, vec![ip("127.0.0.1")]);
    // Configured host_ips win over both.
    let cfg = WebrtcConfig {
        host_ips: vec![ip("84.16.1.2")],
        ..WebrtcConfig::default()
    };
    assert_eq!(
        cfg.candidate_ips(bind_any, &box_addrs, |_| true),
        vec![ip("84.16.1.2")]
    );
    assert!(is_lan_candidate("eth0", ip("192.168.1.10")));
    assert!(!is_lan_candidate("tun0", ip("10.8.0.2")));
    assert!(!is_lan_candidate("eth0", ip("::1")));
}

#[test]
fn unconnected_offers_may_hold_at_most_half_the_slots() {
    assert_eq!(pending_cap(16), 8);
    assert_eq!(pending_cap(4), 2);
    assert_eq!(pending_cap(3), 1);
    assert_eq!(pending_cap(1), 1);
}

#[test]
fn the_rate_limit_is_a_token_bucket_per_client() {
    let t0 = Instant::now();
    let mut r = RateLimiter::new(OFFER_BURST, OFFER_PER_S);
    for _ in 0..5 {
        assert!(r.allow("cf:203.0.113.1", t0));
    }
    assert!(!r.allow("cf:203.0.113.1", t0), "the burst is spent");
    assert!(
        r.allow("cf:203.0.113.2", t0),
        "another client has its own bucket"
    );
    assert!(!r.allow("cf:203.0.113.1", t0 + Duration::from_millis(1900)));
    assert!(
        r.allow("cf:203.0.113.1", t0 + Duration::from_millis(2000)),
        "one every 2 s"
    );
    // A long pause refills to the burst, never beyond it.
    let later = t0 + Duration::from_secs(3600);
    for _ in 0..5 {
        assert!(r.allow("cf:203.0.113.1", later));
    }
    assert!(!r.allow("cf:203.0.113.1", later));
}

#[test]
fn the_rate_limit_key_is_cloudflares_client_then_nginxs_hop_then_the_peer() {
    let peer: Option<IpAddr> = Some("10.77.9.1".parse().unwrap());
    assert_eq!(
        client_key(Some(" 203.0.113.9 "), Some("1.2.3.4"), peer),
        "cf:203.0.113.9"
    );
    // A client cannot pick its key by prepending to X-Forwarded-For: the last hop counts.
    assert_eq!(
        client_key(None, Some("6.6.6.6, 10.77.9.50"), peer),
        "xff:10.77.9.50"
    );
    assert_eq!(client_key(Some(""), None, peer), "peer:10.77.9.1");
    assert_eq!(client_key(None, None, None), "peer:unknown");
}

#[test]
fn refused_offers_are_logged_at_most_every_ten_seconds_with_the_count() {
    let t0 = Instant::now();
    let mut log = RefusalLog::default();
    assert_eq!(log.note(t0), Some(1));
    for i in 1..50 {
        assert_eq!(log.note(t0 + Duration::from_millis(100 * i)), None);
    }
    assert_eq!(log.note(t0 + Duration::from_secs(10)), Some(50));
    assert_eq!(log.note(t0 + Duration::from_secs(11)), None);
}

#[test]
fn only_a_json_body_is_an_offer() {
    assert!(is_json_content_type(Some("application/json")));
    assert!(is_json_content_type(Some(
        "Application/JSON; charset=utf-8"
    )));
    assert!(!is_json_content_type(Some("text/plain")));
    assert!(!is_json_content_type(Some(
        "application/x-www-form-urlencoded"
    )));
    assert!(!is_json_content_type(None));
    assert_eq!(SignalError::RateLimited.status(), 429);
    assert_eq!(SignalError::UnsupportedMediaType.status(), 415);
}

#[test]
fn an_offer_may_name_the_phones_previous_id() {
    let id = PhoneId::from_bytes([7; 16]);
    let body = serde_json::json!({"sdp": "v=0", "replaces": id.to_string()});
    let req = parse_offer_body(&serde_json::to_vec(&body).unwrap()).unwrap();
    assert_eq!(req.replaces, Some(id));
    let body = serde_json::json!({"sdp": "v=0", "replaces": "not an id"});
    let req = parse_offer_body(&serde_json::to_vec(&body).unwrap()).unwrap();
    assert_eq!(req.replaces, None, "a malformed claim is no claim");
}

// intercom#42 review 🔴2: the hub keeps only plausible offer candidates (it is ICE-lite and never
// checks them anyway; phase 3 uses them for TURN permissions).
#[test]
fn implausible_offer_candidates_are_dropped() {
    let own: Vec<SocketAddr> = vec!["10.77.9.202:6992".parse().unwrap()];
    let sdp = [
        "v=0",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111",
        "a=candidate:1 1 udp 2122260223 4f2e1c9a-1234.local 54321 typ host generation 0",
        "a=candidate:2 1 udp 1686052607 84.16.1.2 54322 typ srflx raddr 0.0.0.0 rport 0",
        "a=candidate:3 1 tcp 1518280447 10.0.0.5 9 typ host tcptype active",
        "a=candidate:4 1 udp 2122260223 10.0.0.5 53 typ host",
        "a=candidate:5 1 udp 2122260223 10.77.9.202 6992 typ host",
        "a=candidate:6 1 udp 2122260223 224.0.0.1 40000 typ host",
        "a=candidate:7 1 udp 2122260223 0.0.0.0 40000 typ host",
        "a=candidate:8 1 udp 2122260223 255.255.255.255 40000 typ host",
        "a=candidate:9 1 udp 2122260223 not-an-ip 40000 typ host",
        "a=candidate:10 1 udp 41885439 172.65.1.1 3478 typ relay raddr 84.16.1.2 rport 54322",
        "a=ice-ufrag:abcd",
        "",
    ]
    .join("\r\n");
    let (out, dropped) = filter_offer_candidates(&sdp, &own);
    assert_eq!(dropped, 7, "{out}");
    for kept in ["candidate:1 ", "candidate:2 ", "candidate:10 "] {
        assert!(out.contains(kept), "{kept} kept: {out}");
    }
    for gone in [
        "candidate:3 ",
        "candidate:4 ",
        "candidate:5 ",
        "candidate:6 ",
        "candidate:7 ",
        "candidate:8 ",
        "candidate:9 ",
    ] {
        assert!(!out.contains(gone), "{gone} dropped: {out}");
    }
    assert!(out.starts_with("v=0\r\nm=audio") && out.ends_with("a=ice-ufrag:abcd\r\n"));
    // At most MAX_OFFER_CANDIDATES are kept.
    let many: String = (0..20)
        .map(|i| format!("a=candidate:{i} 1 udp 1 84.16.1.{i} 50000 typ srflx\r\n"))
        .collect();
    let (out, dropped) = filter_offer_candidates(&many, &own);
    assert_eq!(out.matches("a=candidate").count(), MAX_OFFER_CANDIDATES);
    assert_eq!(dropped, 20 - MAX_OFFER_CANDIDATES);
}
