//! intercom#48 phase A: the level mixer's components in a real browser (`wasm-bindgen-test`,
//! headless Chrome or Firefox through `wasm-bindgen-test-runner`). No hub here: the tests mount
//! the board over a `Mixer` they fill themselves, and a strip whose events only reduce (no
//! network). The full page against the real hub is `tests/e2e/levels.spec.js`.
#![cfg(target_arch = "wasm32")]

use intercom_ui::levels::{
    reduce, LevelsBoard, Mixer, SendAction, StripEvent, StripHandle, StripView,
};
use intercom_ui::logic::{meter_view, Level, Output, Status};
use intercom_ui::{App, Page};
use leptos::prelude::*;
use std::sync::{Arc, Mutex};
use wasm_bindgen::JsCast;
use wasm_bindgen_test::*;
use web_sys::{Element, HtmlElement, HtmlInputElement};

wasm_bindgen_test_configure!(run_in_browser);

fn level(name: &str, label: &str, db: f64, muted: bool, master: bool) -> Level {
    Level {
        name: name.into(),
        label: label.into(),
        db,
        muted,
        min_db: -60.0,
        max_db: 12.0,
        master: Some(master),
    }
}

/// A fresh container in the test page's body.
fn container() -> HtmlElement {
    let doc = web_sys::window().unwrap().document().unwrap();
    let div = doc.create_element("div").unwrap();
    doc.body().unwrap().append_child(&div).unwrap();
    div.unchecked_into()
}

fn one(root: &Element, selector: &str) -> Element {
    root.query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("no {selector}"))
}

fn attr(el: &Element, name: &str) -> Option<String> {
    el.get_attribute(name)
}

fn text(el: &Element) -> String {
    el.text_content().unwrap_or_default()
}

async fn settle() {
    leptos::task::tick().await;
    leptos::task::tick().await;
}

#[wasm_bindgen_test]
async fn the_board_has_every_hook_of_the_old_page() {
    let root = container();
    let mixer = Mixer::new();
    let levels = [
        level("foh", "FOH", -3.0, false, false),
        level("cam3", "Kamera 3", 0.0, true, false),
        level("master", "Master", 1.5, false, true),
    ];
    mixer.strips.set(
        levels
            .iter()
            .map(|l| (l.clone(), StripHandle::new(l)))
            .collect(),
    );
    let _mounted = leptos::mount::mount_to(root.clone(), move || {
        view! { <LevelsBoard mixer=mixer version="1.2.3".to_string() /> }
    });
    settle().await;

    assert_eq!(text(&one(&root, "[data-role=version]")), "v1.2.3");
    assert_eq!(text(&one(&root, "h1")), "Hlasitosti v slúchadlách");
    let status = one(&root, "[data-role=status]");
    assert_eq!(attr(&status, "data-state").as_deref(), Some("loading"));
    assert_eq!(text(&status), "Načítavam…");
    assert_eq!(attr(&status, "role").as_deref(), Some("status"));

    let strips = root
        .query_selector_all("[data-role=strips] [data-role=strip]")
        .unwrap();
    assert_eq!(strips.length(), 3);
    let last: Element = strips.item(2).unwrap().unchecked_into();
    assert_eq!(attr(&last, "data-name").as_deref(), Some("master"));
    assert_eq!(attr(&last, "data-master").as_deref(), Some("true"));
    assert!(last.class_list().contains("is-master"));

    let foh = one(&root, "[data-role=strip][data-name=foh]");
    assert_eq!(attr(&foh, "data-master").as_deref(), Some("false"));
    assert_eq!(attr(&foh, "data-db").as_deref(), Some("-3"));
    assert_eq!(attr(&foh, "data-muted").as_deref(), Some("false"));
    assert_eq!(attr(&foh, "data-saved"), None, "no answer yet");
    assert_eq!(text(&one(&foh, "[data-role=db]")), "-3.0 dB");
    assert_eq!(text(&one(&foh, "[data-role=label]")), "FOH");
    let mute = one(&foh, "[data-role=mute]");
    assert_eq!(text(&mute), "Stlmiť");
    assert_eq!(attr(&mute, "aria-pressed").as_deref(), Some("false"));
    // The range: its own range and step, its value set AFTER min/max (else -3 would clamp to 0).
    let slider: HtmlInputElement = one(&foh, "[data-role=slider]").unchecked_into();
    assert_eq!(slider.type_(), "range");
    assert_eq!(slider.min(), "-60");
    assert_eq!(slider.max(), "12");
    assert_eq!(slider.step(), "0.5");
    assert_eq!(slider.value(), "-3");
    assert_eq!(attr(&slider, "aria-label").as_deref(), Some("FOH"));
    one(&foh, ".strip-fader > input.strip-slider"); // the range in its upright box

    let cam3 = one(&root, "[data-role=strip][data-name=cam3]");
    assert!(cam3.class_list().contains("is-muted"));
    assert_eq!(text(&one(&cam3, "[data-role=db]")), "stlmené");
    assert_eq!(text(&one(&cam3, "[data-role=mute]")), "Zapnúť");
    assert_eq!(
        attr(&one(&cam3, "[data-role=mute]"), "aria-pressed").as_deref(),
        Some("true")
    );

    let meter = one(&root, "[data-role=meter]");
    assert_eq!(attr(&meter, "data-db").as_deref(), Some("-120"));
    assert_eq!(text(&one(&root, "[data-role=meter-value]")), "— dB");

    // The status line and the meter follow the page state.
    mixer.status.set(Status::saved(false));
    mixer.meter.set(meter_view(&[Output {
        participant: "cutters".into(),
        out_level_dbfs: Some(-21.5),
    }]));
    mixer.hot.set(Some(false));
    settle().await;
    assert_eq!(attr(&status, "data-state").as_deref(), Some("warn"));
    assert!(text(&status).contains("neuložilo sa"));
    assert_eq!(attr(&meter, "data-db").as_deref(), Some("-21.5"));
    assert_eq!(attr(&meter, "data-hot").as_deref(), Some("false"));
    assert_eq!(text(&one(&root, "[data-role=meter-value]")), "-21 dB");
    let fill: HtmlElement = one(&root, "[data-role=meter-fill]").unchecked_into();
    assert_eq!(fill.style().get_property_value("height").unwrap(), "64.2%");
    root.remove();
}

/// One strip whose events are reduced and recorded, never sent.
async fn strip(
    l: Level,
) -> (
    HtmlElement,
    StripHandle,
    Arc<Mutex<Vec<SendAction>>>,
    impl Drop,
) {
    let root = container();
    let h = StripHandle::new(&l);
    let actions = Arc::new(Mutex::new(Vec::new()));
    let seen = actions.clone();
    let on_event = Callback::new(move |ev: StripEvent| {
        seen.lock().unwrap().push(reduce(h, &ev));
    });
    let mounted = leptos::mount::mount_to(root.clone(), move || {
        view! { <StripView level=l handle=h on_event=on_event /> }
    });
    settle().await;
    (root, h, actions, mounted)
}

#[wasm_bindgen_test]
async fn the_mute_button_toggles_the_strip() {
    let (root, h, actions, _m) = strip(level("click", "Click", 0.0, false, false)).await;
    let el = one(&root, "[data-role=strip]");
    let mute: HtmlElement = one(&root, "[data-role=mute]").unchecked_into();
    mute.click();
    settle().await;
    assert_eq!(attr(&mute, "aria-pressed").as_deref(), Some("true"));
    assert_eq!(text(&mute), "Zapnúť");
    assert_eq!(text(&one(&root, "[data-role=db]")), "stlmené");
    assert_eq!(attr(&el, "data-muted").as_deref(), Some("true"));
    assert!(el.class_list().contains("is-muted"));
    assert_eq!(
        *actions.lock().unwrap(),
        vec![SendAction::Now { cancel: false }]
    );
    assert!(h.state.with_value(|s| s.muted));
    mute.click();
    settle().await;
    assert_eq!(text(&one(&root, "[data-role=db]")), "0.0 dB");
    assert!(!el.class_list().contains("is-muted"));
    root.remove();
}

#[wasm_bindgen_test]
async fn moving_the_fader_shows_it_and_throttles_the_send() {
    let (root, h, actions, _m) = strip(level("foh", "FOH", 0.0, false, false)).await;
    let el = one(&root, "[data-role=strip]");
    let slider: HtmlInputElement = one(&root, "[data-role=slider]").unchecked_into();
    for v in ["5", "6.5"] {
        slider.set_value(v);
        slider
            .dispatch_event(&web_sys::Event::new("input").unwrap())
            .unwrap();
    }
    settle().await;
    assert_eq!(attr(&el, "data-db").as_deref(), Some("6.5"));
    assert_eq!(text(&one(&root, "[data-role=db]")), "+6.5 dB");
    assert_eq!(
        *actions.lock().unwrap(),
        vec![SendAction::Schedule, SendAction::Nothing],
        "one throttled send for a drag"
    );
    slider.set_value("7");
    slider
        .dispatch_event(&web_sys::Event::new("change").unwrap())
        .unwrap();
    settle().await;
    assert_eq!(
        actions.lock().unwrap().last(),
        Some(&SendAction::Now { cancel: true }),
        "the change sends now and cancels the throttled one"
    );
    assert_eq!(attr(&el, "data-db").as_deref(), Some("7"));
    // The hub's value moves the range itself.
    h.apply_server(-12.5, false);
    settle().await;
    assert_eq!(slider.value(), "-12.5");
    assert_eq!(attr(&el, "data-db").as_deref(), Some("-12.5"));
    h.saved.set(Some(false));
    settle().await;
    assert_eq!(attr(&el, "data-saved").as_deref(), Some("false"));
    root.remove();
}

#[wasm_bindgen_test]
async fn the_root_picks_the_page_from_the_shell() {
    assert_eq!(Page::from_attr("levels"), Some(Page::Levels));
    assert_eq!(Page::from_attr("phone"), None, "phase B");
    assert_eq!(Page::from_attr("{{PAGE}}"), None);
    let root = container();
    let _m = leptos::mount::mount_to(root.clone(), || {
        view! { <App page="nothing".to_string() version="1.0.0".to_string() /> }
    });
    settle().await;
    assert_eq!(text(&one(&root, "[data-role=status]")), "Neznáma stránka");
    root.remove();
}
