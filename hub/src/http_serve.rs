//! The HTTP accept loop that closes a connection arriving on the Dante leg at once (intercom#25,
//! [`crate::dante_guard`]).
//!
//! axum 0.7's `axum::serve` has no accept hook: its per-connection `make_service` future is
//! infallible and cannot refuse. This loop accepts itself, asks the guard about the connection's
//! local address, and drops a refused stream before a byte is read. It serves the rest the way
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

/// A failed `accept` (out of file descriptors, a reset before accept) is retried after this pause,
/// as `axum::serve` does.
const ACCEPT_ERROR_PAUSE: Duration = Duration::from_secs(1);

/// Serve `router` on `listener` until `shutdown` completes, then wait for the open connections to
/// finish. A connection whose local address is on the Dante leg is closed at once and counted.
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
                Err(e) => {
                    tracing::warn!(error = ?e, "http: accept failed, retrying in 1 s");
                    tokio::time::sleep(ACCEPT_ERROR_PAUSE).await;
                    continue;
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
