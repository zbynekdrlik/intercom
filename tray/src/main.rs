//! `intercom-tray`: the tray icon that opens the cutters' level page (intercom#34). The pure parts
//! are the library; on Linux `linux.rs` is the GTK + tray-icon shell. Its log goes to stderr (the
//! session unit's journal).

#[cfg(target_os = "linux")]
mod linux;

use std::process::ExitCode;

const USAGE: &str = "usage: intercom-tray [--url http://127.0.0.1:8791/] | --version | --help";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let mut url = intercom_tray::DEFAULT_URL.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--version" => {
                println!("intercom-tray {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--url" => match args.next() {
                Some(u) => url = u,
                None => {
                    eprintln!("--url needs a value\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("unknown argument {other:?}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let control = match intercom_tray::parse_url(&url) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::from(2);
        }
    };
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        url = %control.url,
        status_from = %control.addr,
        "intercom-tray starting"
    );
    run(control)
}

#[cfg(target_os = "linux")]
fn run(control: intercom_tray::ControlUrl) -> ExitCode {
    match linux::run(control) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run(_control: intercom_tray::ControlUrl) -> ExitCode {
    tracing::error!("intercom-tray runs on the Linux strih boxes only");
    ExitCode::FAILURE
}
