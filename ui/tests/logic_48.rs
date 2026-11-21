//! intercom#48 phase A: the level mixer's plain-Rust logic, on the host (`cargo test` in `ui/`).
//! The strings are the ones `web/levels.js` wrote and `tests/e2e/levels.spec.js` reads back.

use intercom_ui::logic::*;

fn level(name: &str, db: f64, muted: bool) -> Level {
    Level {
        name: name.into(),
        label: name.to_uppercase(),
        db,
        muted,
        min_db: -60.0,
        max_db: 12.0,
        master: None,
    }
}

#[test]
fn numbers_read_like_javascripts_string() {
    assert_eq!(js_num(0.0), "0");
    assert_eq!(js_num(-0.0), "0", "String(-0) is 0");
    assert_eq!(js_num(-3.0), "-3");
    assert_eq!(js_num(10.5), "10.5");
    assert_eq!(js_num(-120.0), "-120");
    assert_eq!(js_num(-21.5), "-21.5");
    assert_eq!(js_num(-7.3), "-7.3");
}

#[test]
fn math_round_goes_half_up() {
    assert_eq!(js_round(2.5), 3.0);
    assert_eq!(js_round(-2.5), -2.0);
    assert_eq!(js_round(-2.6), -3.0);
    assert_eq!(js_round(-0.4), 0.0);
}

#[test]
fn the_strip_db_line() {
    assert_eq!(fmt_db(0.0), "0.0 dB");
    assert_eq!(fmt_db(-0.0), "0.0 dB");
    assert_eq!(fmt_db(-0.04), "0.0 dB", "rounds to -0, shown as 0.0");
    assert_eq!(fmt_db(3.5), "+3.5 dB");
    assert_eq!(fmt_db(12.0), "+12.0 dB");
    assert_eq!(fmt_db(-60.0), "-60.0 dB");
    assert_eq!(fmt_db(-7.25), "-7.2 dB", "Math.round(-72.5) = -72");
    assert_eq!(strip_db_text(-3.0, false), "-3.0 dB");
    assert_eq!(strip_db_text(-3.0, true), "stlmené");
    assert_eq!(mute_text(false), "Stlmiť");
    assert_eq!(mute_text(true), "Zapnúť");
}

#[test]
fn the_fader_snaps_to_half_db_and_stays_in_its_range() {
    assert_eq!(fader_db("10.5", -60.0, 12.0, 0.0), 10.5);
    assert_eq!(fader_db("10.4", -60.0, 12.0, 0.0), 10.5);
    assert_eq!(fader_db("10.2", -60.0, 12.0, 0.0), 10.0);
    assert_eq!(fader_db("99", -60.0, 12.0, 0.0), 12.0);
    assert_eq!(fader_db("-99", -60.0, 12.0, 0.0), -60.0);
    assert_eq!(
        fader_db("x", -60.0, 12.0, -4.0),
        -4.0,
        "unparsable keeps the value"
    );
    assert_eq!(fader_db("NaN", -60.0, 12.0, -4.0), -4.0);
    assert_eq!(fader_db("-0", -60.0, 12.0, 1.0), 0.0);
}

#[test]
fn the_meter() {
    let none = meter_view(&[]);
    assert_eq!(none.db_attr, "-120");
    assert_eq!(none.hot, None);
    assert_eq!(none.height, "0%");
    assert_eq!(none.text, "— dB");
    let null = meter_view(&[Output {
        participant: "cutters".into(),
        out_level_dbfs: None,
    }]);
    assert_eq!(null, none);
    let silent = meter_view(&[Output {
        participant: "cutters".into(),
        out_level_dbfs: Some(-120.0),
    }]);
    assert_eq!(silent.db_attr, "-120");
    assert_eq!(silent.text, "ticho");
    assert_eq!(silent.height, "0.0%");
    assert_eq!(silent.hot, Some(false));
    let mid = meter_view(&[Output {
        participant: "cutters".into(),
        out_level_dbfs: Some(-21.5),
    }]);
    assert_eq!(mid.db_attr, "-21.5");
    assert_eq!(mid.text, "-21 dB", "Math.round(-21.5) = -21");
    assert_eq!(mid.height, "64.2%");
    let hot = meter_view(&[Output {
        participant: "cutters".into(),
        out_level_dbfs: Some(-1.0),
    }]);
    assert_eq!(hot.hot, Some(true));
    assert_eq!(hot.text, "-1 dB");
    let over = meter_view(&[Output {
        participant: "x".into(),
        out_level_dbfs: Some(3.0),
    }]);
    assert_eq!(over.height, "100.0%");
}

#[test]
fn the_api_bodies() {
    let body = parse_levels(
        r#"{"version":"1.9.34","levels":[
            {"name":"foh","label":"FOH","dst":"cutters","sources":["foh_dante:1"],"db":-3.5,
             "muted":false,"min_db":-60,"max_db":12,"points":4,"master":false},
            {"name":"master","label":"Master","dst":"cutters","sources":[],"db":0,"muted":true,
             "min_db":-60,"max_db":12,"points":44,"master":true}],
           "outputs":[{"participant":"cutters","out_level_dbfs":null}]}"#,
    )
    .unwrap();
    assert_eq!(body.levels.len(), 2);
    assert!(!body.levels[0].is_master());
    assert!(body.levels[1].is_master());
    assert_eq!(body.levels[0].db, -3.5);
    assert!(body.levels[1].muted);
    assert_eq!(body.outputs[0].out_level_dbfs, None);
    assert!(parse_levels("{").is_err());
    let a = parse_put_answer(r#"{"name":"foh","db":-7.3,"muted":false,"saved":false}"#).unwrap();
    assert_eq!(
        a,
        PutAnswer {
            db: -7.3,
            muted: false,
            saved: false
        }
    );
    let v: serde_json::Value = serde_json::from_str(&put_body(-3.0, true)).unwrap();
    assert_eq!(v, serde_json::json!({"db": -3.0, "muted": true}));
    assert_eq!(level_path("cam3"), "/api/levels/cam3");
    assert_eq!(level_path("a b/c"), "/api/levels/a%20b%2Fc");
}

#[test]
fn the_status_line() {
    assert_eq!(Status::loading().state.as_str(), "loading");
    assert_eq!(Status::loading().text, "Načítavam…");
    assert_eq!(Status::connected().text, "Pripojené");
    assert_eq!(Status::saved(true).text, "Pripojené — uložené");
    assert_eq!(Status::saved(true).state, StatusState::Good);
    assert_eq!(Status::saved(false).state, StatusState::Warn);
    assert!(Status::saved(false).text.contains("neuložilo sa"));
    assert_eq!(Status::unreachable().state.as_str(), "bad");
    assert_eq!(Status::unreachable().text, "Hub nedostupný — skúšam znova");
    assert_eq!(
        Status::send_failed().text,
        "Hub nedostupný — zmena sa neuložila"
    );
    assert_eq!(
        Status::refused("outside -60 .. 12 dB").text,
        "Hub zmenu odmietol: outside -60 .. 12 dB"
    );
    assert_eq!(
        status_after_poll(StatusState::Loading),
        Some(Status::connected())
    );
    assert_eq!(
        status_after_poll(StatusState::Bad),
        Some(Status::connected())
    );
    assert_eq!(status_after_poll(StatusState::Good), None);
    assert_eq!(
        status_after_poll(StatusState::Warn),
        None,
        "keeps 'not saved'"
    );
}

#[test]
fn one_send_in_flight_and_the_last_value_wins() {
    let mut s = StripState::new(&level("foh", 0.0, false));
    assert!(!s.busy());
    s.pointer_down();
    assert!(s.busy());
    assert!(s.input("3"), "the first input schedules the throttled send");
    assert!(!s.input("4"), "a second one waits for it");
    s.timer_fired();
    let first = s.begin_send().unwrap();
    assert_eq!(
        first,
        PutLevel {
            db: 4.0,
            muted: false
        }
    );
    assert!(s.input("5"));
    s.timer_fired();
    assert_eq!(s.begin_send(), None, "one request at a time");
    assert!(s.dirty);
    assert!(!s.change("6"), "no throttled send pending");
    assert!(!s.dragging);
    assert_eq!(s.begin_send(), None);
    // The first answer comes back: not applied (a newer value is pending), resend.
    let e = s.finish_send(SendOutcome::Answer(PutAnswer {
        db: 4.0,
        muted: false,
        saved: true,
    }));
    assert_eq!(e.apply, None);
    assert!(e.resend);
    assert_eq!(e.saved, Some(true));
    assert_eq!(e.status, Status::saved(true));
    assert_eq!(s.db, 6.0, "the page keeps the newer value");
    let second = s.begin_send().unwrap();
    assert_eq!(second.db, 6.0);
    let e = s.finish_send(SendOutcome::Answer(PutAnswer {
        db: 6.0,
        muted: false,
        saved: true,
    }));
    assert_eq!(e.apply, Some((6.0, false)));
    assert!(!e.resend);
    assert!(!s.busy());
}

#[test]
fn a_change_cancels_the_throttled_send_and_sends_now() {
    let mut s = StripState::new(&level("cam3", 0.0, false));
    assert!(s.input("-0.5"));
    assert!(s.change("-0.5"), "the pending throttled send is cancelled");
    assert!(!s.timer);
    assert_eq!(s.begin_send().unwrap().db, -0.5);
}

#[test]
fn a_refused_or_failed_send_keeps_the_page_value() {
    let mut s = StripState::new(&level("click", 0.0, false));
    s.toggle_mute();
    assert!(s.muted);
    s.begin_send().unwrap();
    let e = s.finish_send(SendOutcome::Refused("nope".into()));
    assert_eq!(e.status.state, StatusState::Bad);
    assert_eq!(e.apply, None);
    assert_eq!(e.saved, None);
    assert!(s.muted);
    assert!(!s.inflight);
    s.begin_send().unwrap();
    let e = s.finish_send(SendOutcome::Failed);
    assert_eq!(e.status, Status::send_failed());
    assert!(!e.resend);
}

#[test]
fn an_answer_during_a_drag_is_not_applied() {
    let mut s = StripState::new(&level("lv1", 0.0, false));
    s.begin_send().unwrap();
    s.pointer_down();
    let e = s.finish_send(SendOutcome::Answer(PutAnswer {
        db: -10.0,
        muted: false,
        saved: true,
    }));
    assert_eq!(e.apply, None);
    assert_eq!(s.db, 0.0);
}

#[test]
fn a_poll_never_overwrites_a_busy_strip() {
    let mut s = StripState::new(&level("cam1", 0.0, false));
    assert!(s.poll(&level("cam1", -6.0, true)));
    assert_eq!((s.db, s.muted), (-6.0, true));
    s.pointer_down();
    assert!(!s.poll(&level("cam1", 0.0, false)));
    assert_eq!((s.db, s.muted), (-6.0, true));
    s.pointer_up();
    s.begin_send().unwrap();
    assert!(!s.poll(&level("cam1", 0.0, false)), "in flight");
}

// Review of #48 phase A: a throttle timer the browser refused to set left `timer` true, so that
// strip's `input` never scheduled a send again. The page now calls `schedule_failed` and sends at
// once.
#[test]
fn a_throttle_timer_that_could_not_be_set_never_blocks_the_strip() {
    let mut s = StripState::new(&level("cam2", 0.0, false));
    assert!(s.input("-1"));
    s.schedule_failed();
    assert!(!s.timer, "no timer is pending");
    assert_eq!(s.begin_send().unwrap().db, -1.0, "it is sent at once");
    s.finish_send(SendOutcome::Failed);
    assert!(s.input("-2"), "the next move schedules again");
}
