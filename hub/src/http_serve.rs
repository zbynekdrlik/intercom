//! The HTTP accept loop that closes a connection arriving on the Dante leg at once (intercom#25,
//! [`crate::dante_guard`]).
//!
//! axum 0.7's `axum::serve` has no accept hook: its per-connection `make_service` future is
//! infallible and cannot refuse. This loop accepts itself, asks the guard about the connection's
//! local and peer addresses, and drops a refused stream before a byte is read. It serves the rest the way
//! `axum::serve` does inside: hyper-util's auto (HTTP/1 + HTTP/2) connection with upgrades (the
//! `/ws` and `/janus` WebSocket routes) under a graceful-shutdown watcher.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

use crate::dante_guard::DanteGuard;

/// A failed `accept` other than a per-connection error (out of file descriptors) is retried after
/// this pause, as `axum::serve` does; a per-connection error is retried at once.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_secs(1);

/// Serve `router` on `listener` until `shutdown` completes, then wait for the open connections to
/// finish. A connection whose local or peer address is on the Dante leg is closed at once and
/// counted.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    guard: Arc<DanteGuard>,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let graceful = GracefulShutdown::new();
    let builder = Builder::new(TokioExecutor::new());
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) if is_connection_error(&e) => {
                    tracing::debug!(error = ?e, "http: a connection failed before accept");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = ?e, "http: accept failed, retrying in 1 s");
                    tokio::select! {
                        () = tokio::time::sleep(ACCEPT_ERROR_PAUSE) => continue,
                        () = &mut shutdown => break,
                    }
                }
            },
            () = &mut shutdown => break,
        };
        let local = match stream.local_addr() {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(%peer, error = ?e, "http: no local address for an accepted connection, closing it");
                continue;
            }
        };
        if guard.check_connection(local, peer) {
            drop(stream);
            continue;
        }
        let service = TowerToHyperService::new(router.clone());
        let conn = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), service)
            .into_owned();
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                tracing::debug!(%peer, error = ?e, "http: connection ended with an error");
            }
        });
    }
    drop(listener);
    tracing::info!("http: shutting down, waiting for the open connections");
    graceful.shutdown().await;
    Ok(())
}

/// The accept errors that belong to one connection, not to the listener (axum 0.7.9's split).
fn is_connection_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}
