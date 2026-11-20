//! The strih notebook's tray icon for the intercom hub (intercom#34, owner 19.11.2026: "musi byt
//! tale ikonka na otvorenie dole v bare"). The cutter opens the hub's level page from it instead of
//! typing a URL; it also shows whether the hub answers.
//!
//! This library is the pure part, tested without a desktop: the page's address, the browser it
//! opens, the hub's status read from `/api/version`, the icon's pixels and the session lock's path.
//! `src/linux.rs` is the thin GTK + `tray-icon` shell around it (dstack's dtray pattern, which the
//! strih boxes' tint2 systray already shows).

pub mod mix_window;

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The hub's loopback control listener (`[hub] control_bind`), where the level page lives.
pub const DEFAULT_URL: &str = "http://127.0.0.1:8791/";
/// How long one status read may take, connect and answer each.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest `/api/version` answer read; the real one is about 150 bytes.
const MAX_ANSWER: u64 = 16 * 1024;

/// The page's address and the socket its status is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlUrl {
    /// What the browser opens, exactly as given.
    pub url: String,
    /// The listener `/api/version` is read from.
    pub addr: SocketAddr,
}

/// `http://<loopback ip>:<port>/...` only: the control listener is loopback by design, so any
/// other address is a mistake, refused at start.
pub fn parse_url(url: &str) -> Result<ControlUrl, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("the level page URL must start with http://, got {url:?}"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let addr: SocketAddr = authority.parse().map_err(|_| {
        format!("the level page URL needs a numeric loopback address and port, got {url:?}")
    })?;
    if !addr.ip().is_loopback() {
        return Err(format!(
            "the level page is served on loopback only, got {} in {url:?}",
            addr.ip()
        ));
    }
    Ok(ControlUrl {
        url: url.to_string(),
        addr,
    })
}

/// A program to start and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// Directories searched after `PATH`: the strih boxes' only browser is the snap Firefox
/// (camera-box 19.11.2026), and a session unit's `PATH` may lack `/snap/bin`.
pub const EXTRA_DIRS: &[&str] = &["/snap/bin"];

/// The browser for `url`, the first found of: Firefox in a new window (the strih boxes), a
/// Chrome or Chromium app window (no address bar), else `xdg-open`. `exists` says whether a path is
/// an executable file (the real one checks the file system; tests pass a list).
pub fn choose_launch(
    path_var: Option<&OsStr>,
    url: &str,
    exists: impl Fn(&Path) -> bool,
) -> Launch {
    let mut dirs: Vec<PathBuf> = path_var
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    dirs.extend(EXTRA_DIRS.iter().map(PathBuf::from));
    let find = |name: &str| dirs.iter().map(|d| d.join(name)).find(|p| exists(p));
    if let Some(p) = find("firefox") {
        return Launch {
            program: p,
            args: vec!["--new-window".into(), url.into()],
        };
    }
    for name in ["google-chrome", "chromium", "chromium-browser"] {
        if let Some(p) = find(name) {
            return Launch {
                program: p,
                args: vec![format!("--app={url}")],
            };
        }
    }
    Launch {
        program: find("xdg-open").unwrap_or_else(|| PathBuf::from("xdg-open")),
        args: vec![url.into()],
    }
}

/// Whether `p` is a file with an execute bit (what `choose_launch` is given at run time).
pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// What the tray shows about the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubStatus {
    /// No read finished yet.
    Unknown,
    /// The control listener answered with this version.
    Up(String),
    /// It did not; why.
    Down(String),
}

impl HubStatus {
    /// The menu's status line (Slovak, the cutter reads it).
    pub fn line(&self) -> String {
        match self {
            HubStatus::Unknown => "Hub: zisťujem…".to_string(),
            HubStatus::Up(v) => format!("Hub v{v} beží"),
            HubStatus::Down(why) => format!("Hub nedostupný ({why})"),
        }
    }

    /// The icon's tooltip.
    pub fn tooltip(&self) -> String {
        format!("Interkom: hlasitosti strihača — {}", self.line())
    }

    /// Whether the icon is drawn as up.
    pub fn is_up(&self) -> bool {
        matches!(self, HubStatus::Up(_))
    }
}

/// The status from one finished read.
pub fn status_from(read: Result<String, String>) -> HubStatus {
    match read {
        Ok(v) => HubStatus::Up(v),
        Err(e) => HubStatus::Down(e),
    }
}

/// Reads `GET /api/version` from `addr` (HTTP/1.0, so the hub closes after the answer).
pub fn fetch_version(addr: SocketAddr, timeout: Duration) -> Result<String, String> {
    let mut s = TcpStream::connect_timeout(&addr, timeout).map_err(|e| format!("{addr}: {e}"))?;
    s.set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    // The control listener answers only a loopback Host (its DNS-rebinding guard).
    let req =
        format!("GET /api/version HTTP/1.0\r\nHost: {addr}\r\nAccept: application/json\r\n\r\n");
    s.write_all(req.as_bytes())
        .map_err(|e| format!("{addr}: {e}"))?;
    let mut raw = Vec::new();
    s.take(MAX_ANSWER)
        .read_to_end(&mut raw)
        .map_err(|e| format!("{addr}: {e}"))?;
    parse_version_answer(&raw)
}

/// The version from a raw HTTP answer: status 200 and a JSON body `{"version": "..."}`.
pub fn parse_version_answer(raw: &[u8]) -> Result<String, String> {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "no HTTP answer".to_string())?;
    let status = head.lines().next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if code != "200" {
        return Err(format!("HTTP {}", if code.is_empty() { "?" } else { code }));
    }
    let v: serde_json::Value =
        serde_json::from_str(body.trim()).map_err(|e| format!("not JSON: {e}"))?;
    v.get("version")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| "no version in the answer".to_string())
}

/// The icon's width and height, pixels.
pub const ICON_SIZE: u32 = 32;

/// The icon: a headphone band and two ear cups, green while the hub answers, grey otherwise, on a
/// clear ground. RGBA row by row, [`ICON_SIZE`] × [`ICON_SIZE`]; made in code, no image file.
pub fn icon_rgba(up: bool) -> Vec<u8> {
    let rgb: [u8; 3] = if up { [46, 160, 67] } else { [128, 128, 128] };
    let n = ICON_SIZE as i32;
    let (cx, cy) = (n / 2, n / 2 + 2);
    let mut out = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x - cx, y - cy);
            let d2 = dx * dx + dy * dy;
            // The band: an upper half ring of radius 11..14.
            let band = dy <= 0 && (11 * 11..=14 * 14).contains(&d2);
            // The cups: two blocks at the ring's ends, below its centre line.
            let cup = (2..=8).contains(&(y - cy + 6))
                && ((x - (cx - 13)).abs() <= 2 || (x - (cx + 13)).abs() <= 2);
            let on = band || cup;
            out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], if on { 255 } else { 0 }]);
        }
    }
    out
}

/// The session's single-instance lock file: one tray per desktop session.
pub fn lock_path(runtime_dir: Option<&Path>) -> Option<PathBuf> {
    runtime_dir.map(|d| d.join("intercom-tray.lock"))
}
