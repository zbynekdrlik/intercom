//! The pure parts of the tray's own mix window (intercom#39, owner 20.11.2026: "ten firefox v okne
//! so zalozkami je poriadny hnus"): its title, where its size and position are kept and how that
//! file is read and written, placing it onto a visible monitor, what Open does (raise, create, or
//! the browser fallback) and the Slovak page shown while the level page cannot load.
//! `src/linux_mix.rs` is the GTK + wry shell around it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The window's title (and its taskbar entry's).
pub const WINDOW_TITLE: &str = "Interkom — mix";
/// The size of a window opened for the first time, pixels.
pub const DEFAULT_SIZE: (i32, i32) = (900, 700);
/// The smallest size the window is restored at, pixels.
pub const MIN_SIZE: (i32, i32) = (360, 280);
/// How long the "cannot load" page waits before the level page is loaded again.
pub const RETRY: Duration = Duration::from_secs(3);
/// The largest coordinate or size accepted from the state file (larger = a broken file).
const MAX_COORD: i64 = 100_000;

/// A window's (or a monitor's work area's) position and size, pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn centre(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }
}

/// The state file: `$XDG_STATE_HOME/intercom-tray/window.json`, else
/// `$HOME/.local/state/intercom-tray/window.json`. A relative or empty `XDG_STATE_HOME` is invalid
/// (the XDG spec) and ignored; neither = nowhere to keep it.
pub fn state_path(state_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match state_home.filter(|p| p.is_absolute()) {
        Some(d) => d.to_path_buf(),
        None => home
            .filter(|p| p.is_absolute())?
            .join(".local")
            .join("state"),
    };
    Some(base.join("intercom-tray").join("window.json"))
}

/// The file's text for `r`.
pub fn state_text(r: &Rect) -> String {
    serde_json::json!({"x": r.x, "y": r.y, "width": r.width, "height": r.height}).to_string()
}

/// The state in `text`, or `None` for anything but four whole numbers in range with a positive
/// size.
pub fn parse_state(text: &str) -> Option<Rect> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let num = |k: &str| -> Option<i32> {
        let n = v.get(k)?.as_i64()?;
        if n.abs() > MAX_COORD {
            return None;
        }
        i32::try_from(n).ok()
    };
    let r = Rect {
        x: num("x")?,
        y: num("y")?,
        width: num("width")?,
        height: num("height")?,
    };
    (r.width > 0 && r.height > 0).then_some(r)
}

/// The saved state; a missing, unreadable or broken file is `None` (the window then opens at the
/// default size).
pub fn load_state(path: &Path) -> Option<Rect> {
    parse_state(&std::fs::read_to_string(path).ok()?)
}

/// Writes the state: creates its directory, writes a temporary file beside it and renames it over
/// the old one, so a crash never leaves half a file. The error names the path.
pub fn save_state(path: &Path, r: &Rect) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: no directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = dir.join(".window.json.tmp");
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(state_text(r).as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Where the window opens. `monitors` are the monitors' work areas, the first the primary.
/// - No saved state: the default size, centred on the first monitor.
/// - A saved state: on the monitor its centre is on; a centre on no monitor (e.g. an unplugged
///   one) = centred on the first monitor.
/// - Then the size is kept within [`MIN_SIZE`] .. the monitor and the window is moved fully onto
///   it. No monitor known: the saved state as it is, else the default size at 0,0.
pub fn place(saved: Option<Rect>, monitors: &[Rect]) -> Rect {
    let Some(first) = monitors.first() else {
        return saved.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: DEFAULT_SIZE.0,
            height: DEFAULT_SIZE.1,
        });
    };
    let (mon, mut r, centred) = match saved {
        Some(s) => {
            let (cx, cy) = s.centre();
            match monitors.iter().find(|m| m.contains(cx, cy)) {
                Some(m) => (*m, s, false),
                None => (*first, s, true),
            }
        }
        None => (
            *first,
            Rect {
                x: 0,
                y: 0,
                width: DEFAULT_SIZE.0,
                height: DEFAULT_SIZE.1,
            },
            true,
        ),
    };
    r.width = r.width.max(MIN_SIZE.0).min(mon.width.max(MIN_SIZE.0));
    r.height = r.height.max(MIN_SIZE.1).min(mon.height.max(MIN_SIZE.1));
    if centred {
        r.x = mon.x + (mon.width - r.width) / 2;
        r.y = mon.y + (mon.height - r.height) / 2;
    }
    r.x = r.x.min(mon.x + mon.width - r.width).max(mon.x);
    r.y = r.y.min(mon.y + mon.height - r.height).max(mon.y);
    r
}

/// What the tray's Open does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAction {
    /// Build the window and load the level page.
    Create,
    /// The window exists (maybe hidden or minimised): show it, raise it, focus it.
    Raise,
    /// The WebView could not be built: the browser (`choose_launch`), from then on.
    Browser,
}

/// The choice for Open: an existing window is raised (never a second one); after a WebView that
/// failed to build, the browser, never a retry on every click.
pub fn open_action(window_exists: bool, webview_failed: bool) -> OpenAction {
    if window_exists {
        OpenAction::Raise
    } else if webview_failed {
        OpenAction::Browser
    } else {
        OpenAction::Create
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The page shown while the level page cannot load (the hub is down or restarting): what failed
/// and that it retries every [`RETRY`]. Self-contained, no script (the tray reloads the page).
pub fn error_page(url: &str, reason: &str) -> String {
    format!(
        "<!doctype html><html lang=\"sk\"><head><meta charset=\"utf-8\"><title>{title}</title>\
<style>body{{font:16px system-ui,sans-serif;background:#1b1d21;color:#e8e8e8;display:flex;\
align-items:center;justify-content:center;height:100vh;margin:0}}main{{max-width:30em;padding:1em}}\
h1{{font-size:1.3em;color:#f0a040}}code{{color:#9ab}}small{{color:#999}}</style></head><body><main>\
<h1>Hub sa nedá otvoriť</h1><p>Stránka hlasitostí <code>{url}</code> neodpovedá. Beží \
<code>intercom-hub</code>?</p><p>Skúšam znova o {secs} s…</p><p><small>{reason}</small></p>\
</main></body></html>",
        title = escape(WINDOW_TITLE),
        url = escape(url),
        secs = RETRY.as_secs(),
        reason = escape(reason),
    )
}
