//! The in-hub WebRTC phone leg's pure parts (intercom#42): the `[webrtc]` table and the `webrtc`
//! adapter in `Matrix::from_toml`, the phone ids, the signalling limits, the slot lifecycle and the
//! transport seam the page reads.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use intercom_hub::matrix::{Matrix, ADAPTER_WEBRTC};
use intercom_hub::webrtc_http::transport_json;
use intercom_hub::webrtc_signal::{
    clean_display, parse_display_body, parse_offer_body, PhoneId, SignalError, SlotTable,
    WebrtcConfig, DEFAULT_DISPLAY, DEFAULT_MAX_PHONES, DEFAULT_UDP_BIND, MAX_DISPLAY_CHARS,
    MAX_SDP_BYTES, MAX_SIGNAL_BODY_BYTES,
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
fn host_candidates_come_from_the_config_then_the_bind_then_the_box() {
    let bind_any: SocketAddr = "0.0.0.0:6992".parse().unwrap();
    let lo: IpAddr = "127.0.0.1".parse().unwrap();
    let lan: IpAddr = "10.77.9.202".parse().unwrap();
    let dante: IpAddr = "169.254.10.2".parse().unwrap();
    let dante_routed: IpAddr = "192.168.50.2".parse().unwrap();
    let box_addrs = [lo, lan, dante, dante_routed, lan];
    let cfg = WebrtcConfig::default();
    // Discovered: loopback, link-local and the Dante leg out, duplicates once.
    let got = cfg.candidate_ips(bind_any, &box_addrs, |ip| ip == dante_routed);
    assert_eq!(got, vec![lan]);
    // A specific bind is the only candidate.
    let got = cfg.candidate_ips("127.0.0.1:6992".parse().unwrap(), &box_addrs, |_| false);
    assert_eq!(got, vec![lo]);
    // Configured host_ips win over both.
    let cfg = WebrtcConfig {
        host_ips: vec![dante_routed],
        ..WebrtcConfig::default()
    };
    assert_eq!(
        cfg.candidate_ips(bind_any, &box_addrs, |_| true),
        vec![dante_routed]
    );
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
