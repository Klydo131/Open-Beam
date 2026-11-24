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

/// The address limits apply to. One IPv6 network (a /64) usually belongs
/// to one household or machine, which can pick any address inside it, so
/// the whole network counts as one; an IPv4 address written as IPv6 counts
/// as itself.
pub fn client(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None if v6.is_loopback() => ip,
            None => {
                let s = v6.segments();
                IpAddr::V6(std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
    }
}

/// Open connections, in total and per address, and the reverse proxies
/// trusted to name the visitor's address.
#[derive(Default)]
pub struct Connections {
    open: Mutex<(usize, HashMap<IpAddr, usize>)>,
    trusted: Vec<IpAddr>,
}

/// True when `peer` is one of the configured reverse proxies.
fn is_trusted(trusted: &[IpAddr], peer: IpAddr) -> bool {
    let peer = peer.to_canonical();
    trusted.iter().any(|t| t.to_canonical() == peer)
}

/// The visitor's address. A request from a trusted reverse proxy names it
/// in `X-Forwarded-For`; the list is read from the right, skipping the
/// trusted proxies themselves, because only the entries our own proxies
/// appended can be believed: a client can put anything at the left. From
/// any other peer the header is ignored.
pub fn visitor(peer: IpAddr, headers: &axum::http::HeaderMap, trusted: &[IpAddr]) -> IpAddr {
    if !is_trusted(trusted, peer) {
        return peer;
    }
    let mut hops = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .collect::<Vec<_>>();
    while let Some(hop) = hops.pop() {
        match hop.parse::<IpAddr>() {
            Ok(ip) if is_trusted(trusted, ip) => continue,
            Ok(ip) => return ip,
            // Something unreadable where an address should be: the chain
            // cannot be trusted past this point.
            Err(_) => return peer,
        }
    }
    peer
}

/// One admitted connection; gives its place back when dropped.
pub struct Place {
    table: Arc<Connections>,
    from: IpAddr,
}

impl Connections {
    pub fn with_trusted_proxies(trusted: Vec<IpAddr>) -> Self {
        Self {
            open: Mutex::default(),
            trusted,
        }
    }

    /// Admits a connection from `from`, or refuses it when the server or
    /// that address already has as many as allowed.
    pub fn admit(self: &Arc<Self>, from: IpAddr) -> Option<Place> {
        // A reverse proxy carries everyone's connections, so like this
        // machine it has only the overall limit.
        let exempt = from.is_loopback() || is_trusted(&self.trusted, from);
        let from = client(from);
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        let mine = open.1.get(&from).copied().unwrap_or(0);
        if open.0 >= MAX_CONNECTIONS || (mine >= MAX_PER_ADDRESS && !exempt) {
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
        let table = connections.clone();
        let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
            let from = visitor(address.ip(), request.headers(), &table.trusted);
            // A proxy that did not say who it is forwarding for is not
            // the visitor; counting it would let a few failures lock
            // everyone out, so such a request carries no address.
            if !is_trusted(&table.trusted, from) {
                request.extensions_mut().insert(Peer(client(from)));
            }
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

/// Time to send the body of anything but a file upload: forms and API
/// messages are small, so a body still arriving after this is being dripped.
pub const BODY_TIME: Duration = Duration::from_secs(30);
/// Largest body read up front for requests other than file uploads; each
/// handler applies its own, smaller limit.
const SMALL_BODY: usize = 4 * 1024 * 1024;

/// Ends any request that takes longer than [`REQUEST_TIME`]. The body of
/// anything but a file upload must arrive within [`BODY_TIME`], so slowly
/// dripped form posts cannot hold connections for the full request time.
pub async fn request_time_limit(request: Request, next: Next) -> Response {
    let upload = request.method() == axum::http::Method::POST && request.uri().path() == "/data";
    let request = if upload {
        request
    } else {
        let (parts, body) = request.into_parts();
        match tokio::time::timeout(BODY_TIME, axum::body::to_bytes(body, SMALL_BODY)).await {
            Ok(Ok(bytes)) => Request::from_parts(parts, Body::from(bytes)),
            Ok(Err(_)) => return (StatusCode::PAYLOAD_TOO_LARGE, "The request is too large.\n").into_response(),
            Err(_) => return (StatusCode::REQUEST_TIMEOUT, "The request took too long to arrive.\n").into_response(),
        }
    };
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
    fn forwarded_addresses_are_believed_only_from_trusted_proxies() {
        use axum::http::{HeaderMap, HeaderValue};
        let proxy: IpAddr = "10.0.0.5".parse().unwrap();
        let trusted = [proxy];
        let mut headers = HeaderMap::new();
        // The client claims to be 1.1.1.1; the proxy appended the real one.
        headers.insert("x-forwarded-for", HeaderValue::from_static("1.1.1.1, 203.0.113.9"));
        assert_eq!(
            visitor(proxy, &headers, &trusted),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // From anyone else the header is ignored.
        let stranger: IpAddr = "198.51.100.7".parse().unwrap();
        assert_eq!(visitor(stranger, &headers, &trusted), stranger);
        assert_eq!(
            visitor(proxy, &headers, &[]),
            proxy,
            "no proxies configured: header ignored"
        );
        // A chain of our own proxies is skipped from the right.
        let inner: IpAddr = "10.0.0.6".parse().unwrap();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9, 10.0.0.6"));
        assert_eq!(
            visitor(proxy, &headers, &[proxy, inner]),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // Garbage stops the walk instead of trusting what lies beyond it.
        headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.9, not-an-address"),
        );
        assert_eq!(visitor(proxy, &headers, &trusted), proxy);
        // An IPv4-mapped peer still matches its IPv4 entry.
        let mapped: IpAddr = "::ffff:10.0.0.5".parse().unwrap();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));
        assert_eq!(
            visitor(mapped, &headers, &trusted),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // A trusted proxy is not capped per address.
        let table = Arc::new(Connections::with_trusted_proxies(vec![proxy]));
        let held: Vec<Place> = (0..MAX_PER_ADDRESS * 2).map(|_| table.admit(proxy).unwrap()).collect();
        assert_eq!(held.len(), MAX_PER_ADDRESS * 2);
    }

    #[test]
    fn an_ipv6_network_counts_as_one_address() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:1234:5678:9abc".parse().unwrap();
        let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(client(a), client(b));
        assert_ne!(client(a), client(other));
        let mapped: IpAddr = "::ffff:203.0.113.7".parse().unwrap();
        assert_eq!(client(mapped), "203.0.113.7".parse::<IpAddr>().unwrap());
        assert!(client("::1".parse().unwrap()).is_loopback());
        let table = Arc::new(Connections::default());
        let held: Vec<Place> = (0..MAX_PER_ADDRESS as u16)
            .map(|n| {
                table
                    .admit(IpAddr::V6(std::net::Ipv6Addr::new(0x2001, 0xdb8, 1, 2, 0, 0, 0, n)))
                    .unwrap()
            })
            .collect();
        assert!(
            table.admit(b).is_none(),
            "rotating addresses inside one network does not help"
        );
        drop(held);
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
