//! The Linux shell around the library: `tray-icon`'s GTK backend shows the icon through
//! AppIndicator (the strih boxes' tint2 systray shows dstack's dtray the same way). A thread reads
//! the hub's `/api/version` every [`POLL`]; GTK's loop shows the result and handles the menu.
//! One tray per session (a lock in `XDG_RUNTIME_DIR`). Open shows the tray's own mix window
//! (`linux_mix`, intercom#39) in this same GTK loop; the browser is only its fallback.

use std::fs::{File, OpenOptions, TryLockError};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use glib::ControlFlow;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use intercom_tray::{
    choose_launch, fetch_version, icon_rgba, is_executable, lock_path, status_from, ControlUrl,
    HubStatus, ICON_SIZE, POLL_TIMEOUT,
};

/// How often the hub is read.
const POLL: Duration = Duration::from_secs(2);
/// How often GTK shows the last read and handles the menu's clicks.
const TICK: Duration = Duration::from_millis(200);

fn single_instance() -> Result<Option<File>, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let path = lock_path(dir.as_deref())
        .ok_or_else(|| "XDG_RUNTIME_DIR is not set: no user session".to_string())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| format!("the lock {}: {e}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(format!("the lock {}: {e}", path.display())),
    }
}

fn icon(up: bool) -> Result<Icon, String> {
    Icon::from_rgba(icon_rgba(up), ICON_SIZE, ICON_SIZE).map_err(|e| format!("the icon: {e}"))
}

/// The fallback: starts the browser on the page and logs how it ended (its own thread reaps it).
fn open_in_browser(url: &str) {
    let launch = choose_launch(std::env::var_os("PATH").as_deref(), url, is_executable);
    tracing::info!(program = %launch.program.display(), args = ?launch.args, "opening the level page");
    match Command::new(&launch.program).args(&launch.args).spawn() {
        Ok(mut child) => {
            let program = launch.program.display().to_string();
            std::thread::spawn(move || match child.wait() {
                Ok(st) if st.success() => tracing::debug!(%program, "browser launcher ended"),
                Ok(st) => tracing::warn!(%program, status = %st, "browser launcher failed"),
                Err(e) => tracing::warn!(%program, error = %e, "browser launcher not reaped"),
            });
        }
        Err(e) => {
            tracing::error!(program = %launch.program.display(), error = %e, "could not start the browser")
        }
    }
}

pub fn run(control: ControlUrl, open_at_start: bool) -> Result<(), String> {
    // The X class of the mix window (its taskbar entry groups by it), set before GTK starts.
    glib::set_prgname(Some("intercom-tray"));
    glib::set_application_name("Interkom");
    gtk::init().map_err(|e| format!("no desktop to show the tray on (GTK): {e}"))?;
    let Some(_lock) = single_instance()? else {
        tracing::info!("another intercom-tray runs in this session; leaving");
        return Ok(());
    };

    let status_item = MenuItem::new(HubStatus::Unknown.line(), false, None);
    let open_item = MenuItem::new("Otvoriť hlasitosti", true, None);
    let copy_item = MenuItem::new("Kopírovať URL", true, None);
    let exit_item = MenuItem::new("Ukončiť ikonu (hub beží ďalej)", true, None);
    let menu = Menu::new();
    let e = |r: Result<(), tray_icon::menu::Error>| r.map_err(|e| format!("the menu: {e}"));
    e(menu.append(&status_item))?;
    e(menu.append(&PredefinedMenuItem::separator()))?;
    e(menu.append(&open_item))?;
    e(menu.append(&copy_item))?;
    e(menu.append(&PredefinedMenuItem::separator()))?;
    e(menu.append(&exit_item))?;
    let tray: TrayIcon = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(HubStatus::Unknown.tooltip())
        .with_icon(icon(false)?)
        .build()
        .map_err(|e| format!("the tray icon: {e}"))?;

    let latest = Arc::new(Mutex::new(HubStatus::Unknown));
    {
        let (latest, addr) = (Arc::clone(&latest), control.addr);
        std::thread::Builder::new()
            .name("hub-status".into())
            .spawn(move || loop {
                let s = status_from(fetch_version(addr, POLL_TIMEOUT));
                *latest.lock().unwrap_or_else(|p| p.into_inner()) = s;
                std::thread::sleep(POLL);
            })
            .map_err(|e| format!("the status thread: {e}"))?;
    }

    let mut shown = HubStatus::Unknown;
    let url = control.url.clone();
    let mix = crate::linux_mix::MixWindow::new(&url);
    if open_at_start {
        mix.open(open_in_browser);
    }
    glib::timeout_add_local(TICK, move || {
        let now = latest.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if now != shown {
            match &now {
                HubStatus::Up(v) => tracing::info!(version = %v, "hub answers"),
                HubStatus::Down(why) => tracing::warn!(reason = %why, "hub does not answer"),
                HubStatus::Unknown => {}
            }
            status_item.set_text(now.line());
            if let Err(e) = tray.set_tooltip(Some(now.tooltip())) {
                tracing::warn!(error = %e, "tooltip not set");
            }
            if now.is_up() != shown.is_up() {
                match icon(now.is_up()) {
                    Ok(i) => {
                        if let Err(e) = tray.set_icon(Some(i)) {
                            tracing::warn!(error = %e, "icon not set");
                        }
                    }
                    Err(e) => tracing::warn!("{e}"),
                }
            }
            shown = now;
        }
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *open_item.id() {
                mix.open(open_in_browser);
            } else if ev.id == *copy_item.id() {
                gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD).set_text(&url);
                tracing::info!(%url, "URL copied");
            } else if ev.id == *exit_item.id() {
                tracing::info!("exit from the menu (the hub keeps running)");
                mix.save();
                gtk::main_quit();
                return ControlFlow::Break;
            }
        }
        ControlFlow::Continue
    });
    gtk::main();
    Ok(())
}
