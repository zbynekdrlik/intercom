//! intercom#34: the tray's pure parts — the page's address, the browser it opens, the hub's status
//! read from `/api/version` (against a real local listener), the icon and the lock's path.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use intercom_tray::*;

#[test]
fn the_default_url_is_the_hubs_loopback_control_listener() {
    let c = parse_url(DEFAULT_URL).unwrap();
    assert_eq!(c.url, "http://127.0.0.1:8791/");
    assert_eq!(c.addr, "127.0.0.1:8791".parse().unwrap());
}

#[test]
fn a_url_off_loopback_or_without_http_or_a_numeric_port_is_refused() {
    for bad in [
        "https://127.0.0.1:8791/",
        "http://10.77.9.202:8791/",
        "http://strih-snv:8791/",
        "http://127.0.0.1/",
        "127.0.0.1:8791",
    ] {
        assert!(parse_url(bad).is_err(), "{bad} must be refused");
    }
    assert_eq!(
        parse_url("http://[::1]:9000/x").unwrap().addr,
        "[::1]:9000".parse().unwrap()
    );
}

fn launch_with(present: &[&str], path: &str) -> Launch {
    let present: Vec<PathBuf> = present.iter().map(PathBuf::from).collect();
    let path = OsString::from(path);
    choose_launch(Some(&path), DEFAULT_URL, |p: &Path| {
        present.iter().any(|q| q == p)
    })
}

#[test]
fn the_strih_boxes_snap_firefox_opens_a_new_window_even_off_path() {
    // camera-box 19.11.2026: the only browser is /snap/bin/firefox, and the unit's PATH may lack it.
    let l = launch_with(&["/snap/bin/firefox", "/usr/bin/xdg-open"], "/usr/bin:/bin");
    assert_eq!(l.program, PathBuf::from("/snap/bin/firefox"));
    assert_eq!(l.args, ["--new-window", "http://127.0.0.1:8791/"]);
}

#[test]
fn firefox_on_path_wins_over_chromium() {
    let l = launch_with(&["/usr/bin/firefox", "/usr/bin/chromium"], "/usr/bin");
    assert_eq!(l.program, PathBuf::from("/usr/bin/firefox"));
}

#[test]
fn without_firefox_chrome_or_chromium_opens_an_app_window() {
    let l = launch_with(&["/opt/x/chromium-browser"], "/usr/bin:/opt/x");
    assert_eq!(l.program, PathBuf::from("/opt/x/chromium-browser"));
    assert_eq!(l.args, ["--app=http://127.0.0.1:8791/"]);
    let l = launch_with(&["/usr/bin/google-chrome", "/usr/bin/chromium"], "/usr/bin");
    assert_eq!(l.program, PathBuf::from("/usr/bin/google-chrome"));
}

#[test]
fn no_browser_falls_back_to_xdg_open() {
    let l = launch_with(&["/usr/bin/xdg-open"], "/usr/bin");
    assert_eq!(l.program, PathBuf::from("/usr/bin/xdg-open"));
    assert_eq!(l.args, ["http://127.0.0.1:8791/"]);
    let l = launch_with(&[], "/usr/bin");
    assert_eq!(
        l.program,
        PathBuf::from("xdg-open"),
        "the bare name, so the spawn error names it"
    );
}

#[test]
fn only_an_executable_file_counts() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("intercom-tray-exec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("firefox");
    std::fs::write(&f, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!is_executable(&f));
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(is_executable(&f));
    assert!(!is_executable(&dir), "a directory is not a browser");
    assert!(!is_executable(&dir.join("missing")));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_status_lines_say_what_the_cutter_needs() {
    assert_eq!(HubStatus::Unknown.line(), "Hub: zisťujem…");
    let up = status_from(Ok("1.9.26".into()));
    assert_eq!(up, HubStatus::Up("1.9.26".into()));
    assert_eq!(up.line(), "Hub v1.9.26 beží");
    assert!(up.is_up());
    assert!(up.tooltip().contains("Hub v1.9.26 beží"));
    let down = status_from(Err("127.0.0.1:8791: Connection refused".into()));
    assert_eq!(
        down.line(),
        "Hub nedostupný (127.0.0.1:8791: Connection refused)"
    );
    assert!(!down.is_up());
    assert!(!HubStatus::Unknown.is_up());
}

#[test]
fn an_answer_is_parsed_only_when_it_is_a_200_with_a_version() {
    let ok = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"version\":\"1.9.26\"}";
    assert_eq!(parse_version_answer(ok).unwrap(), "1.9.26");
    assert_eq!(
        parse_version_answer(b"HTTP/1.1 403 Forbidden\r\n\r\nno").unwrap_err(),
        "HTTP 403"
    );
    assert!(parse_version_answer(b"HTTP/1.1 200 OK\r\n\r\nnot json")
        .unwrap_err()
        .starts_with("not JSON"));
    assert_eq!(
        parse_version_answer(b"HTTP/1.1 200 OK\r\n\r\n{\"v\":1}").unwrap_err(),
        "no version in the answer"
    );
    assert_eq!(
        parse_version_answer(b"garbage").unwrap_err(),
        "no HTTP answer"
    );
}

/// A one-shot HTTP server on a free loopback port: returns its address and the request it saw.
fn serve_once(answer: &'static [u8]) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut buf = [0u8; 1024];
        let n = s.read(&mut buf).unwrap();
        s.write_all(answer).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    (addr, h)
}

#[test]
fn the_status_read_asks_the_listener_with_a_loopback_host_and_reads_its_version() {
    let (addr, h) = serve_once(b"HTTP/1.1 200 OK\r\n\r\n{\"version\":\"1.9.26\"}");
    assert_eq!(
        fetch_version(addr, Duration::from_secs(2)).unwrap(),
        "1.9.26"
    );
    let req = h.join().unwrap();
    assert!(req.starts_with("GET /api/version HTTP/1.0\r\n"), "{req}");
    // The hub's control listener refuses a non-loopback Host (its DNS-rebinding guard).
    assert!(req.contains(&format!("Host: {addr}\r\n")), "{req}");
}

#[test]
fn a_refused_connection_is_a_down_status_naming_the_address() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let e = fetch_version(addr, Duration::from_millis(500)).unwrap_err();
    assert!(e.starts_with(&addr.to_string()), "{e}");
}

#[test]
fn the_icon_is_a_full_rgba_square_green_when_up_grey_when_down() {
    let n = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let up = icon_rgba(true);
    let down = icon_rgba(false);
    assert_eq!((up.len(), down.len()), (n, n));
    let lit = |px: &[u8]| px.chunks(4).filter(|p| p[3] == 255).count();
    assert!(lit(&up) > 60, "the headphones are drawn");
    assert_eq!(lit(&up), lit(&down), "the same shape both ways");
    assert!(up.chunks(4).any(|p| p[3] == 0), "a clear ground");
    let first_lit = |px: &[u8]| px.chunks(4).find(|p| p[3] == 255).unwrap().to_vec();
    assert_eq!(&first_lit(&up)[..3], &[46, 160, 67]);
    assert_eq!(&first_lit(&down)[..3], &[128, 128, 128]);
}

#[test]
fn the_lock_lives_in_the_sessions_runtime_dir() {
    assert_eq!(
        lock_path(Some(Path::new("/run/user/1000"))),
        Some(PathBuf::from("/run/user/1000/intercom-tray.lock"))
    );
    assert_eq!(lock_path(None), None);
}

#[test]
fn the_tray_version_equals_the_workspace_version() {
    // The release ships both binaries from one tag; the tray is its own workspace (no inheritance).
    let root =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../Cargo.toml")).unwrap();
    let want = root
        .lines()
        .skip_while(|l| l.trim() != "[workspace.package]")
        .find_map(|l| {
            l.trim()
                .strip_prefix("version = ")
                .map(|v| v.trim_matches('"').to_string())
        })
        .expect("the root workspace version");
    assert_eq!(env!("CARGO_PKG_VERSION"), want);
}
