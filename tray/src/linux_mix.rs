//! The tray's own mix window (intercom#39): a plain `gtk::Window` titled "Interkom — mix" with
//! wry's WebKitGTK WebView of the level page, built into the tray's GTK loop on its thread
//! (`build_gtk`, no second event loop). Open raises the one window; closing it only hides it; its
//! size and position are kept in the state file; while the page cannot load it shows a Slovak
//! page and retries. The decisions are `intercom_tray::mix_window` (tested); this is the GTK shell.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::prelude::*;
use webkit2gtk::{LoadEvent, NetworkError, WebViewExt};
use wry::{WebViewBuilder, WebViewBuilderExtUnix, WebViewExtUnix};

use intercom_tray::mix_window::{
    error_page, load_state, open_action, place, save_state, state_path, OpenAction, Rect, RETRY,
    WINDOW_TITLE,
};
use intercom_tray::{icon_rgba, ICON_SIZE};

/// How long after the last move or resize the state is written.
const SAVE_DELAY: Duration = Duration::from_secs(1);

struct Shown {
    window: gtk::Window,
    /// Kept alive with the window: dropping it would tear the page down.
    _webview: wry::WebView,
}

/// The one mix window, created on the first Open. Lives on the GTK thread only.
pub struct MixWindow {
    url: String,
    state_file: Option<PathBuf>,
    shown: RefCell<Option<Shown>>,
    webview_failed: Cell<bool>,
    /// The last size and position seen, written to the state file.
    last: Rc<RefCell<Option<Rect>>>,
    save_pending: Rc<Cell<bool>>,
}

impl MixWindow {
    pub fn new(url: &str) -> Rc<Self> {
        let state_file = state_path(
            std::env::var_os("XDG_STATE_HOME")
                .as_deref()
                .map(std::path::Path::new),
            std::env::var_os("HOME")
                .as_deref()
                .map(std::path::Path::new),
        );
        if state_file.is_none() {
            tracing::warn!("neither XDG_STATE_HOME nor HOME: the mix window's size is not kept");
        }
        Rc::new(Self {
            url: url.to_string(),
            state_file,
            shown: RefCell::new(None),
            webview_failed: Cell::new(false),
            last: Rc::new(RefCell::new(None)),
            save_pending: Rc::new(Cell::new(false)),
        })
    }

    /// The tray's Open: raise the window, create it, or (the WebView failed) the browser.
    pub fn open(self: &Rc<Self>, browser: impl Fn(&str)) {
        let exists = self.shown.borrow().is_some();
        match open_action(exists, self.webview_failed.get()) {
            OpenAction::Raise => {
                if let Some(s) = self.shown.borrow().as_ref() {
                    tracing::info!("raising the mix window");
                    if !s.window.is_visible() {
                        // Shown again where it was hidden (a window manager may not keep it).
                        if let Some(r) = *self.last.borrow() {
                            s.window.move_(r.x, r.y);
                            s.window.resize(r.width, r.height);
                        }
                    }
                    s.window.show_all();
                    s.window.deiconify();
                    s.window.present();
                }
            }
            OpenAction::Create => match self.create() {
                Ok(shown) => {
                    shown.window.present();
                    *self.shown.borrow_mut() = Some(shown);
                }
                Err(e) => {
                    tracing::error!(error = %e, "the mix window cannot show the page; the browser from now on");
                    self.webview_failed.set(true);
                    browser(&self.url);
                }
            },
            OpenAction::Browser => browser(&self.url),
        }
    }

    /// Writes the last size and position (on hide, on exit, after a move).
    pub fn save(&self) {
        let (Some(path), Some(r)) = (self.state_file.as_ref(), *self.last.borrow()) else {
            return;
        };
        match save_state(path, &r) {
            Ok(()) => tracing::debug!(path = %path.display(), ?r, "mix window state saved"),
            Err(e) => tracing::warn!(error = %e, "mix window state not saved"),
        }
    }

    fn create(self: &Rc<Self>) -> Result<Shown, String> {
        let saved = self.state_file.as_deref().and_then(load_state);
        let r = place(saved, &monitors());
        tracing::info!(?saved, placed = ?r, url = %self.url, "creating the mix window");

        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title(WINDOW_TITLE);
        window.set_default_size(r.width, r.height);
        window.move_(r.x, r.y);
        window.set_icon(Some(&window_icon()));
        let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.add(&vbox);

        let webview = WebViewBuilder::new()
            .with_url(&self.url)
            .build_gtk(&vbox)
            .map_err(|e| format!("the WebView: {e}"))?;
        let page = webview.webview();
        let url = self.url.clone();
        page.connect_load_failed(move |page, _ev: LoadEvent, failing, err| {
            // A load replaced by another (the retry, a click) is not a failure.
            if err.matches(NetworkError::Cancelled) {
                return false;
            }
            tracing::warn!(uri = %failing, error = %err, "the level page did not load; retrying");
            page.load_html(&error_page(&url, err.message()), None);
            let (page, url) = (page.clone(), url.clone());
            glib::timeout_add_local_once(RETRY, move || page.load_uri(&url));
            true
        });

        *self.last.borrow_mut() = Some(r);
        {
            let (last, pending, me) = (
                Rc::clone(&self.last),
                Rc::clone(&self.save_pending),
                Rc::downgrade(self),
            );
            window.connect_configure_event(move |w, _| {
                // Hiding the window configures it too, with GTK's stale first position: only a
                // shown window's moves count.
                if !w.is_visible() {
                    return false;
                }
                let (x, y) = w.position();
                let (width, height) = w.size();
                *last.borrow_mut() = Some(Rect {
                    x,
                    y,
                    width,
                    height,
                });
                if !pending.replace(true) {
                    let (pending, me) = (Rc::clone(&pending), me.clone());
                    glib::timeout_add_local_once(SAVE_DELAY, move || {
                        pending.set(false);
                        if let Some(me) = me.upgrade() {
                            me.save();
                        }
                    });
                }
                false
            });
        }
        {
            let me = Rc::downgrade(self);
            window.connect_delete_event(move |w, _| {
                tracing::info!("mix window closed: hidden, the tray keeps running");
                if let Some(me) = me.upgrade() {
                    me.save();
                }
                w.hide();
                glib::Propagation::Stop
            });
        }
        window.show_all();
        Ok(Shown {
            window,
            _webview: webview,
        })
    }
}

/// The monitors' work areas, the primary first (empty without a display).
fn monitors() -> Vec<Rect> {
    let Some(display) = gtk::gdk::Display::default() else {
        return Vec::new();
    };
    let primary = display.primary_monitor();
    let mut out: Vec<(bool, Rect)> = (0..display.n_monitors())
        .filter_map(|i| display.monitor(i))
        .map(|m| {
            let a = m.workarea();
            let is_primary = primary.as_ref() == Some(&m);
            (
                is_primary,
                Rect {
                    x: a.x(),
                    y: a.y(),
                    width: a.width(),
                    height: a.height(),
                },
            )
        })
        .collect();
    out.sort_by_key(|(p, _)| !p);
    out.into_iter().map(|(_, r)| r).collect()
}

/// The tray's headphones (drawn as up) as the window's and its taskbar entry's icon.
fn window_icon() -> Pixbuf {
    let n = ICON_SIZE as i32;
    let bytes = glib::Bytes::from_owned(icon_rgba(true));
    Pixbuf::from_bytes(&bytes, Colorspace::Rgb, true, 8, n, n, n * 4)
}
