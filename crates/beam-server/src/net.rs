//! Accepting connections without letting any client hold the server.
//!
//! Three limits, each against a known way of tying a server up without ever
//! signing in:
//!
//! - A client must finish sending a request's headers within
//!   [`HEADER_TIME`]. Sending them a byte at a time ("Slowloris") no longer
//!   keeps a connection open forever.
//! - At most [`MAX_CONNECTIONS`] connections are open at once, and at most
//!   [`MAX_PER_ADDRESS`] from one address, so one machine cannot take every
//!   place. Connections over the limit are closed at once. Addresses on
//!   this machine have only the overall limit: behind a reverse proxy,
//!   every visitor arrives from one of them.
//! - A request, body included, must be answered within [`REQUEST_TIME`].

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tower::ServiceExt;

pub const HEADER_TIME: Duration = Duration::from_secs(10);
pub const REQUEST_TIME: Duration = Duration::from_secs(10 * 60);
pub const MAX_CONNECTIONS: usize = 1024;
pub const MAX_PER_ADDRESS: usize = 64;
/// How long open requests may finish after the server is told to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// The address a request came from. Absent in tests that call the router
/// directly. Behind a reverse proxy this is the proxy's address.
#[derive(Clone, Copy, Debug)]
pub struct Peer(pub IpAddr);

/// Open connections, in total and per address.
#[derive(Default)]
pub struct Connections {
    open: Mutex<(usize, HashMap<IpAddr, usize>)>,
}

/// One admitted connection; gives its place back when dropped.
pub struct Place {
    table: Arc<Connections>,
    from: IpAddr,
}

impl Connections {
    /// Admits a connection from `from`, or refuses it when the server or
    /// that address already has as many as allowed.
    pub fn admit(self: &Arc<Self>, from: IpAddr) -> Option<Place> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let mine = open.1.get(&from).copied().unwrap_or(0);
        if open.0 >= MAX_CONNECTIONS || (mine >= MAX_PER_ADDRESS && !from.is_loopback()) {
            return None;
        }
        open.0 += 1;
        open.1.insert(from, mine + 1);
        Some(Place {
            table: self.clone(),
            from,
        })
    }

    pub fn count(&self) -> usize {
        self.open.lock().unwrap_or_else(|e| e.into_inner()).0
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut open = self.table.open.lock().unwrap_or_else(|e| e.into_inner());
        open.0 = open.0.saturating_sub(1);
        if let Some(n) = open.1.get_mut(&self.from) {
            *n -= 1;
            if *n == 0 {
                open.1.remove(&self.from);
            }
        }
    }
}

/// Serves `app` on `listener` until `stop` completes, then lets open
/// requests finish for a few seconds.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    connections: Arc<Connections>,
    stop: impl std::future::Future<Output = ()>,
) -> std::io::Result<()> {
    let graceful = hyper_util::server::graceful::GracefulShutdown::new();
    tokio::pin!(stop);
    loop {
        let (stream, address): (_, SocketAddr) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                // Running out of file handles or a reset connection must not
                // stop the server; pause briefly so the loop does not spin.
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
            },
            () = &mut stop => break,
        };
        let Some(place) = connections.admit(address.ip()) else {
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        let peer = Peer(address.ip());
        let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
            request.extensions_mut().insert(peer);
            app.clone().oneshot(request.map(Body::new))
        });
        let connection = hyper::server::conn::http1::Builder::new()
            .timer(TokioTimer::new())
            .header_read_timeout(HEADER_TIME)
            .serve_connection(TokioIo::new(stream), service);
        let connection = graceful.watch(connection);
        tokio::spawn(async move {
            let _ = connection.await;
            drop(place);
        });
    }
    drop(listener);
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()).await;
    Ok(())
}

/// Ends any request that takes longer than [`REQUEST_TIME`], including one
/// whose body is being sent very slowly.
pub async fn request_time_limit(request: Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_TIME, next.run(request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::REQUEST_TIMEOUT, "The request took too long.\n").into_response(),
    }
}

/// `seconds` since 1970 as an RFC 3339 time in UTC, e.g.
/// `2026-05-22T08:00:00Z`.
pub fn rfc3339(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_written_as_rfc_3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
    }

    #[test]
    fn one_address_cannot_take_every_place() {
        let table = Arc::new(Connections::default());
        let busy: IpAddr = "203.0.113.7".parse().unwrap();
        let other: IpAddr = "198.51.100.2".parse().unwrap();
        let held: Vec<Place> = (0..MAX_PER_ADDRESS).map(|_| table.admit(busy).unwrap()).collect();
        assert!(table.admit(busy).is_none(), "the busy address is at its limit");
        assert!(table.admit(other).is_some(), "everyone else still gets in");
        drop(held);
        assert_eq!(table.count(), 0, "places are given back");
        assert!(table.admit(busy).is_some());
        let proxy: IpAddr = "127.0.0.1".parse().unwrap();
        let through_proxy: Vec<Place> = (0..MAX_PER_ADDRESS * 2).map(|_| table.admit(proxy).unwrap()).collect();
        assert_eq!(
            through_proxy.len(),
            MAX_PER_ADDRESS * 2,
            "a reverse proxy is not capped per address"
        );
    }
}
