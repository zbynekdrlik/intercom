//! The phone-side Janus WebSocket allowlist (issue 1453, `intercom_hub::janus_ws_guard`): every
//! frame shape the phone page really sends is forwarded, and the plain-RTP join (the UDP reflection
//! the owner's ROZHODNUTÉ 18.10.2026 refuses on the public path) and every other request the page
//! never sends are refused with a Janus-shaped error. Pure (serde_json only), so it also runs in a
//! plain-rustc replica (Tier-0 #557).

use serde_json::{json, Value};

use intercom_hub::janus_ws_guard::{
    check_client_frame, Refusal, Verdict, ALLOWED_JANUS_REQUESTS, AUDIOBRIDGE_PLUGIN,
    CONFIGURE_KEYS, JANUS_ERROR_INVALID_JSON, JANUS_ERROR_INVALID_JSON_OBJECT,
    JANUS_ERROR_UNAUTHORIZED, JANUS_WS_PROTOCOL, JOIN_KEYS,
};

const ROOM: u64 = 1000;

/// The raw text of one frame of every shape the phone page sends (captured from the phone E2E
/// suite; phone.spec.js fails when the page sends a shape that is not in this file).
const PWA_FRAMES: &str = include_str!("../../tests/fixtures/janus-ws-pwa-frames.json");

fn pwa_frames() -> Vec<String> {
    serde_json::from_str(PWA_FRAMES).expect("the sample is a JSON array of frame strings")
}

fn refused(text: &str) -> Refusal {
    match check_client_frame(text, ROOM) {
        Verdict::Refuse(r) => r,
        Verdict::Forward => panic!("forwarded a frame the allowlist must refuse: {text}"),
    }
}

fn reply(r: &Refusal) -> Value {
    serde_json::from_str(&r.reply).expect("the refusal reply is JSON")
}

fn message(body: Value) -> String {
    json!({
        "janus": "message",
        "body": body,
        "transaction": "tx-1453",
        "session_id": 1001,
        "handle_id": 1002
    })
    .to_string()
}

#[test]
fn the_constants_match_janus_and_the_page() {
    assert_eq!(JANUS_WS_PROTOCOL, "janus-protocol");
    assert_eq!(AUDIOBRIDGE_PLUGIN, "janus.plugin.audiobridge");
    assert_eq!(JANUS_ERROR_UNAUTHORIZED, 403);
    assert_eq!(JANUS_ERROR_INVALID_JSON, 454);
    assert_eq!(JANUS_ERROR_INVALID_JSON_OBJECT, 455);
    assert!(!JOIN_KEYS.contains(&"rtp") && !CONFIGURE_KEYS.contains(&"rtp"));
    assert!(!ALLOWED_JANUS_REQUESTS.contains(&"claim"));
}

#[test]
fn every_frame_the_phone_page_sends_is_forwarded() {
    let frames = pwa_frames();
    assert!(frames.len() >= 12, "the sample covers the page's shapes");
    let mut kinds = std::collections::BTreeSet::new();
    for text in &frames {
        assert_eq!(
            check_client_frame(text, ROOM),
            Verdict::Forward,
            "the page's own frame is refused: {text}"
        );
        let v: Value = serde_json::from_str(text).unwrap();
        kinds.insert(v["janus"].as_str().unwrap().to_string());
    }
    // Every janus request janus.js can send is in the sample.
    for k in ALLOWED_JANUS_REQUESTS {
        assert!(kinds.contains(k), "the sample has no {k} frame");
    }
}

#[test]
fn a_plain_rtp_join_is_refused_with_a_janus_error_for_its_transaction() {
    let r = refused(&message(json!({
        "request": "join", "room": ROOM, "display": "x", "muted": true,
        "rtp": {"ip": "203.0.113.7", "port": 9}
    })));
    assert_eq!(r.code, JANUS_ERROR_UNAUTHORIZED);
    assert!(r.reason.contains("plain-RTP"), "{}", r.reason);
    let v = reply(&r);
    assert_eq!(v["janus"], "error");
    assert_eq!(v["transaction"], "tx-1453");
    assert_eq!(v["session_id"], 1001);
    assert_eq!(v["error"]["code"], 403);
    assert_eq!(v["error"]["reason"], r.reason.as_str());
    assert!(r.reason.starts_with("interkom: "));
}

#[test]
fn rtp_is_refused_in_every_request_that_can_carry_it() {
    for request in ["join", "configure", "changeroom"] {
        let r = refused(&message(json!({
            "request": request, "room": ROOM, "rtp": {"ip": "203.0.113.7", "port": 9}
        })));
        assert_eq!(r.code, JANUS_ERROR_UNAUTHORIZED, "{request}");
    }
    // Even an empty rtp object, and alone.
    refused(&message(
        json!({"request": "join", "room": ROOM, "rtp": {}}),
    ));
    refused(&message(json!({"request": "configure", "rtp": null})));
}

#[test]
fn every_audiobridge_request_but_join_and_configure_is_refused() {
    for request in [
        "create",
        "edit",
        "destroy",
        "rtp_forward",
        "stop_rtp_forward",
        "listforwarders",
        "play_file",
        "stop_file",
        "enable_recording",
        "enable_mjrs",
        "changeroom",
        "leave",
        "kick",
        "kick_all",
        "mute",
        "unmute",
        "mute_room",
        "allowed",
        "suspend",
        "list",
        "listparticipants",
        "exists",
    ] {
        let r = refused(&message(json!({"request": request, "room": ROOM})));
        assert_eq!(r.code, JANUS_ERROR_UNAUTHORIZED, "{request}");
        assert!(
            r.reason.contains("is not allowed"),
            "{request}: {}",
            r.reason
        );
    }
}

#[test]
fn a_create_of_an_rtp_room_is_refused() {
    // The bypass the per-room switch alone would leave: a room of the attacker's own.
    let r = refused(&message(json!({
        "request": "create", "room": 4242, "allow_rtp_participants": true
    })));
    assert_eq!(r.code, JANUS_ERROR_UNAUTHORIZED);
}

#[test]
fn a_body_key_the_page_never_sends_is_refused() {
    for (request, key, value) in [
        ("configure", "record", json!(true)),
        ("configure", "filename", json!("/tmp/x")),
        ("join", "secret", json!("s")),
        ("join", "admin_key", json!("k")),
        ("join", "id", json!(7)),
        ("join", "codec", json!("pcmu")),
        ("configure", "group", json!("g")),
    ] {
        let mut body = json!({"request": request, "room": ROOM});
        body[key] = value;
        let r = refused(&message(body));
        assert!(r.reason.contains("may not carry"), "{key}: {}", r.reason);
    }
}

#[test]
fn a_join_into_another_room_is_refused() {
    refused(&message(json!({"request": "join", "room": 4242})));
    refused(&message(json!({"request": "join", "room": "1000"})));
    refused(&message(json!({"request": "join"})));
    refused(&message(json!({"request": "join", "room": -1})));
    assert_eq!(
        check_client_frame(&message(json!({"request": "join", "room": ROOM})), ROOM),
        Verdict::Forward
    );
    // The hub's own room decides, not a constant.
    assert_eq!(
        check_client_frame(&message(json!({"request": "join", "room": 7})), 7),
        Verdict::Forward
    );
}

#[test]
fn wrong_value_types_are_refused() {
    refused(&message(json!({"request": "configure", "muted": "false"})));
    refused(&message(
        json!({"request": "configure", "display": {"a": 1}}),
    ));
    refused(&message(
        json!({"request": "join", "room": ROOM, "display": 5}),
    ));
    refused(&message(json!({"request": 5})));
}

#[test]
fn a_message_without_a_body_object_is_refused() {
    refused(&json!({"janus": "message", "transaction": "t"}).to_string());
    refused(&json!({"janus": "message", "body": "join", "transaction": "t"}).to_string());
}

#[test]
fn an_attach_to_any_other_plugin_is_refused() {
    for plugin in [
        "janus.plugin.videoroom",
        "janus.plugin.streaming",
        "janus.plugin.sip",
        "janus.plugin.recordplay",
        "",
    ] {
        let r = refused(
            &json!({"janus": "attach", "plugin": plugin, "transaction": "t", "session_id": 1})
                .to_string(),
        );
        assert_eq!(r.code, JANUS_ERROR_UNAUTHORIZED, "{plugin}");
    }
    refused(&json!({"janus": "attach", "transaction": "t"}).to_string());
}

#[test]
fn a_janus_request_janus_js_never_sends_is_refused() {
    for janus in [
        "claim",
        "ping",
        "add_token",
        "list_sessions",
        "rtp_forward",
        "",
    ] {
        let r = refused(&json!({"janus": janus, "transaction": "t"}).to_string());
        assert_eq!(r.code, JANUS_ERROR_UNAUTHORIZED, "{janus}");
    }
    refused(&json!({"transaction": "t"}).to_string());
    refused(&json!({"janus": 1, "transaction": "t"}).to_string());
}

#[test]
fn not_json_and_not_an_object_get_the_janus_json_errors_without_a_transaction() {
    let r = refused("{not json");
    assert_eq!(r.code, JANUS_ERROR_INVALID_JSON);
    let v = reply(&r);
    assert_eq!(v["error"]["code"], 454);
    assert!(v.get("transaction").is_none());

    let r = refused(r#"[{"janus":"create","transaction":"t"}]"#);
    assert_eq!(r.code, JANUS_ERROR_INVALID_JSON_OBJECT);
    assert_eq!(reply(&r)["error"]["code"], 455);
}

#[test]
fn the_reply_echoes_a_large_session_id_exactly_and_only_a_string_transaction() {
    let text = r#"{"janus":"attach","plugin":"janus.plugin.videoroom","transaction":"tx","session_id":9007199254740993}"#;
    let r = refused(text);
    assert!(r.reply.contains("9007199254740993"), "{}", r.reply);
    let r = refused(r#"{"janus":"claim","transaction":7,"session_id":"x"}"#);
    let v = reply(&r);
    assert!(v.get("transaction").is_none() && v.get("session_id").is_none());
}

#[test]
fn a_client_chosen_name_is_cut_and_escaped_in_the_reason() {
    let long = format!("evil\n{}", "x".repeat(200));
    let r = refused(&json!({"janus": long, "transaction": "t"}).to_string());
    assert!(
        !r.reason.contains('\n'),
        "a control character reached the reason"
    );
    assert!(r.reason.contains("\\n"));
    assert!(r.reason.len() < 120, "{} chars", r.reason.len());
}

#[test]
fn a_forwarded_frame_is_decided_on_content_not_on_formatting() {
    // Whitespace and key order of the client's own text do not matter to the decision.
    let text =
        "{ \"body\" : {\"muted\":false , \"request\":\"configure\"},\n\"janus\":\"message\" }";
    assert_eq!(check_client_frame(text, ROOM), Verdict::Forward);
}

/// The guard and Janus must read a frame the same way, or a frame could pass the guard and reach
/// Janus as something else. Both parsers keep the LAST of duplicate keys (serde_json's map insert,
/// jansson without JSON_REJECT_DUPLICATES) and decode `\u` escapes before keys are compared; the
/// guard matches names exactly where Janus compares `request` case-insensitively (stricter).
#[test]
fn the_guard_reads_a_frame_the_way_janus_does() {
    let frame = |body: &str| {
        format!(
            r#"{{"janus":"message","transaction":"t","session_id":1,"handle_id":2,"body":{body}}}"#
        )
    };
    // An escaped key or value is the same key or value once decoded (JSON \uXXXX escapes). The
    // escapes are written with an escaped backslash so no tool decodes them on the way in; the
    // asserts prove the frame text really carries them.
    let escaped_key = frame(
        "{\"request\":\"join\",\"room\":1000,\"r\\u0074p\":{\"ip\":\"203.0.113.7\",\"port\":9}}",
    );
    assert!(escaped_key.contains("r\\u0074p"), "{escaped_key}");
    refused(&escaped_key);
    let escaped_request = frame(
        "{\"request\":\"j\\u006fin\",\"room\":1000,\"rtp\":{\"ip\":\"203.0.113.7\",\"port\":9}}",
    );
    assert!(escaped_request.contains("j\\u006fin"), "{escaped_request}");
    refused(&escaped_request);
    let escaped_join = frame("{\"request\":\"j\\u006fin\",\"room\":1000}");
    assert!(escaped_join.contains("j\\u006fin"), "{escaped_join}");
    assert_eq!(
        check_client_frame(&escaped_join, ROOM),
        Verdict::Forward,
        "an escaped plain join is read as a join, as Janus reads it"
    );
    // Duplicate keys: the LAST one decides, for the guard as for Janus.
    refused(&frame(
        r#"{"request":"join","room":1000,"request":"rtp_forward"}"#,
    ));
    assert_eq!(
        check_client_frame(
            &frame(r#"{"request":"rtp_forward","request":"join","room":1000}"#),
            ROOM
        ),
        Verdict::Forward,
        "the last request is a plain join, as Janus reads it"
    );
    refused(&frame(
        r#"{"request":"join","room":1000,"rtp":{"ip":"x","port":9},"rtp":null}"#,
    ));
    refused(&frame(r#"{"request":"join","room":4242,"room":1001}"#));
    refused(&frame(
        r#"{"request":"configure","muted":true,"muted":"no"}"#,
    ));
    // The top-level request too.
    refused(r#"{"janus":"message","janus":"claim","transaction":"t"}"#);
    // Numbers that are not an integer room.
    refused(&frame(r#"{"request":"join","room":1000.0}"#));
    refused(&frame(r#"{"request":"join","room":1e3}"#));
    // Case variants Janus would accept are refused here.
    refused(r#"{"janus":"MESSAGE","transaction":"t","body":{"request":"join","room":1000}}"#);
    refused(&frame(r#"{"request":"Join","room":1000}"#));
    refused(r#"{"janus":"attach","plugin":"Janus.Plugin.AudioBridge","transaction":"t"}"#);
    // Trailing data after the object.
    refused(r#"{"janus":"keepalive","transaction":"t"} {"janus":"claim"}"#);
}
