//! intercom#39: the tray's own mix window — its pure parts: where its size and position are kept,
//! the file's read and write, placing it onto a visible monitor, raise vs create vs the browser
//! fallback, and the Slovak page shown while the level page cannot load.

use std::path::{Path, PathBuf};

use intercom_tray::mix_window::*;

fn r(x: i32, y: i32, width: i32, height: i32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

/// A fresh empty directory under the target dir (no tempfile crate in this workspace).
fn fresh_dir(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "tray39-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn the_window_is_titled_for_the_cutter() {
    assert_eq!(WINDOW_TITLE, "Interkom — mix");
}

#[test]
fn the_state_lives_under_xdg_state_home_else_the_homes_local_state() {
    assert_eq!(
        state_path(Some(Path::new("/run/s")), Some(Path::new("/home/c"))),
        Some(PathBuf::from("/run/s/intercom-tray/window.json"))
    );
    assert_eq!(
        state_path(None, Some(Path::new("/home/c"))),
        Some(PathBuf::from("/home/c/.local/state/intercom-tray/window.json"))
    );
    // The XDG spec: a relative XDG_STATE_HOME is invalid and ignored.
    assert_eq!(
        state_path(Some(Path::new("rel/dir")), Some(Path::new("/home/c"))),
        Some(PathBuf::from("/home/c/.local/state/intercom-tray/window.json"))
    );
    assert_eq!(state_path(Some(Path::new("")), None), None);
    assert_eq!(state_path(None, None), None);
}

#[test]
fn the_state_text_round_trips() {
    let s = r(-40, 25, 1024, 768);
    assert_eq!(parse_state(&state_text(&s)), Some(s));
}

#[test]
fn a_broken_or_partial_or_nonsense_state_is_none() {
    for bad in [
        "",
        "not json",
        "{}",
        r#"{"x": 1, "y": 2, "width": 300}"#,
        r#"{"x": 1, "y": 2, "width": "300", "height": 200}"#,
        r#"{"x": 1, "y": 2, "width": 0, "height": 200}"#,
        r#"{"x": 1, "y": 2, "width": 300, "height": -5}"#,
        r#"{"x": 1e12, "y": 2, "width": 300, "height": 200}"#,
    ] {
        assert_eq!(parse_state(bad), None, "{bad:?} must not be a state");
    }
}

#[test]
fn save_creates_the_directory_and_load_reads_it_back() {
    let dir = fresh_dir("save");
    let path = dir.join("intercom-tray").join("window.json");
    let s = r(100, 50, 800, 600);
    save_state(&path, &s).unwrap();
    assert_eq!(load_state(&path), Some(s));
    // Saving again replaces it, and leaves no temporary file behind.
    let t = r(0, 0, 640, 480);
    save_state(&path, &t).unwrap();
    assert_eq!(load_state(&path), Some(t));
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["window.json"]);
}

#[test]
fn a_missing_or_corrupt_file_loads_as_none() {
    let dir = fresh_dir("load");
    assert_eq!(load_state(&dir.join("nope.json")), None);
    let p = dir.join("window.json");
    std::fs::write(&p, "{\"x\": ").unwrap();
    assert_eq!(load_state(&p), None);
}

#[test]
fn a_save_that_cannot_write_says_where() {
    let dir = fresh_dir("ro");
    // The "directory" is a plain file, so creating the state directory fails.
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let e = save_state(&blocker.join("window.json"), &r(0, 0, 400, 300)).unwrap_err();
    assert!(e.contains("blocker"), "{e}");
}

const FULL_HD: Rect = Rect {
    x: 0,
    y: 0,
    width: 1920,
    height: 1080,
};

#[test]
fn without_a_saved_state_the_default_size_is_centred_on_the_first_monitor() {
    let p = place(None, &[FULL_HD]);
    assert_eq!((p.width, p.height), DEFAULT_SIZE);
    assert_eq!(p.x, (1920 - DEFAULT_SIZE.0) / 2);
    assert_eq!(p.y, (1080 - DEFAULT_SIZE.1) / 2);
}

#[test]
fn a_saved_state_on_a_monitor_is_kept_as_it_was() {
    let s = r(200, 100, 800, 600);
    assert_eq!(place(Some(s), &[FULL_HD]), s);
    // On the second monitor too.
    let second = r(1920, 0, 1280, 1024);
    let t = r(2100, 300, 700, 500);
    assert_eq!(place(Some(t), &[FULL_HD, second]), t);
}

#[test]
fn a_window_left_on_an_unplugged_monitor_comes_back_centred_on_a_visible_one() {
    // Saved on a second monitor at x 1920.., which is gone now.
    let p = place(Some(r(2200, 200, 800, 600)), &[FULL_HD]);
    assert_eq!(p, r(560, 240, 800, 600));
}

#[test]
fn a_window_hanging_over_the_edge_is_pulled_fully_onto_its_monitor() {
    // Its centre is on the monitor, its right and bottom edges are not.
    let p = place(Some(r(1500, 700, 800, 600)), &[FULL_HD]);
    assert_eq!(p, r(1120, 480, 800, 600));
    let q = place(Some(r(-300, -100, 800, 600)), &[FULL_HD]);
    assert_eq!(q, r(0, 0, 800, 600));
}

#[test]
fn a_window_larger_than_its_monitor_or_tiny_is_resized_into_range() {
    let small = r(0, 0, 1280, 720);
    let p = place(Some(r(0, 0, 3000, 2000)), &[small]);
    assert_eq!(p, small);
    let q = place(Some(r(10, 10, 20, 10)), &[small]);
    assert_eq!((q.width, q.height), MIN_SIZE);
    assert_eq!((q.x, q.y), (10, 10));
}

#[test]
fn without_any_monitor_the_state_is_used_unchanged_or_the_default() {
    let s = r(5, 6, 700, 500);
    assert_eq!(place(Some(s), &[]), s);
    assert_eq!(place(None, &[]), r(0, 0, DEFAULT_SIZE.0, DEFAULT_SIZE.1));
}

#[test]
fn open_raises_an_existing_window_creates_a_missing_one_and_falls_back_after_a_failure() {
    assert_eq!(open_action(false, false), OpenAction::Create);
    assert_eq!(open_action(true, false), OpenAction::Raise);
    // The WebView could not be built before: the browser from then on, never a retry loop.
    assert_eq!(open_action(false, true), OpenAction::Browser);
}

#[test]
fn the_page_down_message_is_slovak_names_the_address_and_says_it_retries() {
    let html = error_page("http://127.0.0.1:8791/", "Connection refused");
    assert!(html.contains("<meta charset=\"utf-8\">"), "{html}");
    assert!(html.contains("Hub sa nedá otvoriť"), "{html}");
    assert!(html.contains("http://127.0.0.1:8791/"), "{html}");
    assert!(html.contains("Connection refused"), "{html}");
    assert!(
        html.contains(&format!("Skúšam znova o {} s", RETRY.as_secs())),
        "{html}"
    );
}

#[test]
fn the_page_down_message_escapes_what_it_shows() {
    let html = error_page("http://127.0.0.1:8791/?a=<b>&c=\"d\"", "<script>x</script>");
    assert!(!html.contains("<script>"), "{html}");
    assert!(!html.contains("<b>"), "{html}");
    assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "{html}");
    assert!(html.contains("a=&lt;b&gt;&amp;c=&quot;d&quot;"), "{html}");
}

#[test]
fn it_retries_every_few_seconds() {
    assert!((1..=10).contains(&RETRY.as_secs()), "{RETRY:?}");
}
