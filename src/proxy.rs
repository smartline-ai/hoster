use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::header::{CONNECTION, HOST, HeaderValue, UPGRADE};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use crate::routing::{RouteState, SharedRoutes};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The scheme the client actually reached the proxy on. Threaded from the
/// accepting listener into [`handle`] so `x-forwarded-proto` reflects
/// reality rather than being hardcoded: the plain listener passes `Http`,
/// the TLS listener passes `Https`. An application behind the proxy that
/// checks this header (Rails' `force_ssl`, Django, Next.js, anything that
/// builds absolute URLs) needs the true answer — a wrong one causes
/// redirect loops or mixed-content-blocked links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn header_value(self) -> HeaderValue {
        match self {
            Scheme::Http => HeaderValue::from_static("http"),
            Scheme::Https => HeaderValue::from_static("https"),
        }
    }
}

/// Response body type. Upstream bodies stream through; error pages are `Full`.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

/// Client used to reach containers. Plain HTTP: TLS terminates at the proxy.
pub type HttpClient = Client<HttpConnector, Incoming>;

pub fn build_client() -> HttpClient {
    let mut connector = HttpConnector::new();
    connector.set_keepalive(Some(std::time::Duration::from_secs(30)));
    Client::builder(TokioExecutor::new()).build(connector)
}

fn text(status: StatusCode, body: &'static str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never: Infallible| match never {})
                .boxed(),
        )
        .expect("static response is always valid")
}

/// True when the client asked to leave HTTP behind (websockets, mostly).
///
/// `Connection` is a comma-separated list and both header values are
/// case-insensitive, so this cannot be an equality check.
fn is_upgrade_request(req: &Request<Incoming>) -> bool {
    let connection_has_upgrade = req
        .headers()
        .get(CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
        })
        .unwrap_or(false);

    connection_has_upgrade && req.headers().contains_key(UPGRADE)
}

/// The standard RFC 7230 §6.1 hop-by-hop header names, compared
/// case-insensitively against lowercase header names as returned by `hyper`.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Removes the standard hop-by-hop headers plus any header named as a token
/// in the request's `Connection` value (RFC 7230 §6.1). Callers that need to
/// preserve an upgrade must save what they need before calling this and
/// re-insert it after.
fn strip_hop_by_hop(headers: &mut hyper::HeaderMap) {
    let connection_listed: Vec<String> = headers
        .get(CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| {
            v.split(',')
                .map(|t| t.trim().to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();

    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    for name in connection_listed {
        headers.remove(name.as_str());
    }
}

/// Accept loop. Runs until the process ends. Plain HTTP: every connection is
/// tagged `Scheme::Http`, so `x-forwarded-proto` is always `http` — this is
/// the byte-identical behaviour the plain listener had before TLS support
/// existed.
pub async fn serve(listener: TcpListener, routes: SharedRoutes) -> anyhow::Result<()> {
    serve_with_scheme(listener, routes, Scheme::Http).await
}

/// The shared accept-loop body behind [`serve`]. Not used for the TLS
/// listener in `main.rs`, which needs its own loop to perform the TLS
/// handshake before a connection can be treated as HTTP — but it exists here
/// (rather than only inline in `serve`) so tests can exercise `handle` with
/// `Scheme::Https` without standing up a real TLS listener.
async fn serve_with_scheme(
    listener: TcpListener,
    routes: SharedRoutes,
    scheme: Scheme,
) -> anyhow::Result<()> {
    let client = build_client();
    tracing::info!(addr = %listener.local_addr()?, "proxy listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // Per-connection accept errors (fd limits, resets) must never
                // kill the listener — every other branch depends on it.
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };

        let keepalive = socket2::TcpKeepalive::new().with_time(std::time::Duration::from_secs(30));
        if let Err(e) = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
            tracing::debug!(error = %e, "could not set tcp keepalive");
        }

        let routes = routes.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let service =
                service_fn(move |req| handle(req, routes.clone(), client.clone(), scheme));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await
            {
                tracing::debug!(%peer, error = %e, "connection closed with error");
            }
        });
    }
}

/// The whole request path: look up the host, forward, or explain why not.
///
/// Returns `Infallible` because every failure is a response. A proxy that
/// returns an error to hyper drops the connection, which tells the user
/// nothing.
pub async fn handle(
    mut req: Request<Incoming>,
    routes: SharedRoutes,
    client: HttpClient,
    scheme: Scheme,
) -> Result<Response<ProxyBody>, Infallible> {
    let Some(host) = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
    else {
        return Ok(text(StatusCode::BAD_REQUEST, "missing Host header"));
    };

    let route = {
        let table = routes.load();
        match table.lookup(&host) {
            Some(r) => r.clone(),
            None => {
                tracing::debug!(%host, "no route");
                return Ok(text(StatusCode::NOT_FOUND, "unknown host"));
            }
        }
    };

    if route.state == RouteState::Starting {
        return Ok(text(StatusCode::SERVICE_UNAVAILABLE, "starting"));
    }

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let upstream_uri: Uri = match format!("http://{}{}", route.upstream, path_and_query).parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(%host, error = %e, "could not build upstream uri");
            return Ok(text(StatusCode::BAD_REQUEST, "bad request target"));
        }
    };
    *req.uri_mut() = upstream_uri;

    // Save what the upgrade handshake needs before stripping hop-by-hop
    // headers — `connection` and `upgrade` are both in the strip set, so a
    // naive strip would silently kill websockets.
    let upgrading = is_upgrade_request(&req);
    let upgrade_hdr = req.headers().get(UPGRADE).cloned();

    strip_hop_by_hop(req.headers_mut());

    if upgrading {
        req.headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("upgrade"));
        if let Some(v) = upgrade_hdr {
            req.headers_mut().insert(UPGRADE, v);
        }
    }

    // The app needs to know the name the browser used — it generates absolute
    // URLs from it. The original Host header is left intact; these are the
    // conventional extras. This happens after the strip so the forwarded
    // headers survive it.
    if let Ok(v) = HeaderValue::from_str(&host) {
        req.headers_mut().insert("x-forwarded-host", v);
    }
    req.headers_mut()
        .insert("x-forwarded-proto", scheme.header_value());

    // Take the client's upgrade future out of the request before forwarding.
    // It resolves only after we return the 101, so it must be captured now and
    // awaited later.
    let client_upgrade = upgrading.then(|| hyper::upgrade::on(&mut req));

    let mut upstream_resp = match client.request(req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!(%host, upstream = %route.upstream, error = %e, "upstream failed");
            return Ok(text(StatusCode::BAD_GATEWAY, "upstream unavailable"));
        }
    };

    if upstream_resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some(client_upgrade) = client_upgrade else {
            // Upstream switched protocols on a request that never asked to.
            tracing::warn!(%host, "unexpected 101 from upstream");
            return Ok(text(StatusCode::BAD_GATEWAY, "upstream protocol error"));
        };
        let upstream_upgrade = hyper::upgrade::on(&mut upstream_resp);

        // Both sides finish upgrading only after the 101 below is written, so
        // this waits off to the side.
        tokio::spawn(async move {
            let (client_io, upstream_io) = match tokio::try_join!(client_upgrade, upstream_upgrade)
            {
                Ok(pair) => pair,
                Err(e) => {
                    tracing::debug!(error = %e, "upgrade failed");
                    return;
                }
            };
            let mut client_io = TokioIo::new(client_io);
            let mut upstream_io = TokioIo::new(upstream_io);
            // From here it is opaque bytes in both directions until someone
            // hangs up. Errors are routine (tab closed) — log at debug.
            // A half-open peer (killed container, network partition) with no
            // clean FIN/RST would otherwise block both reads forever and pin
            // this task for the process lifetime; TCP keepalive on both the
            // client-facing socket (serve()) and the upstream connector
            // (build_client()) bounds that by surfacing a socket error here.
            if let Err(e) = tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await {
                tracing::debug!(error = %e, "tunnel closed");
            }
        });

        // Hand the 101 back with the upstream's headers so the client agrees on
        // the protocol. The body is empty: the bytes flow through the tunnel.
        let mut resp = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        for (name, value) in upstream_resp.headers() {
            resp = resp.header(name, value);
        }
        return Ok(resp
            .body(
                Full::new(Bytes::new())
                    .map_err(|never: Infallible| match never {})
                    .boxed(),
            )
            .expect("101 response is always valid"));
    }

    Ok(upstream_resp.map(|b| b.map_err(BoxError::from).boxed()))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::routing::{Route, RoutingTable};

    /// A minimal upstream that records the `x-forwarded-proto` header of the
    /// most recent request it received, and replies `200 OK`.
    async fn spawn_proto_capturing_upstream() -> (std::net::SocketAddr, Arc<Mutex<Option<String>>>)
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(None));
        let seen_for_task = seen.clone();

        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let seen = seen_for_task.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let seen = seen.clone();
                        async move {
                            let proto = req
                                .headers()
                                .get("x-forwarded-proto")
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string);
                            *seen.lock().unwrap() = proto;
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Full::new(Bytes::from("ok")))
                                    .unwrap(),
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        (addr, seen)
    }

    /// Starts `serve_with_scheme` on an ephemeral port and returns its base
    /// URL — this drives `handle` directly through the real accept loop, the
    /// same one `serve()` (plain) uses, parameterized with `scheme` so the
    /// TLS listener's tagging can be exercised without standing up real TLS.
    async fn spawn_proxy_with_scheme(routes: SharedRoutes, scheme: Scheme) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = serve_with_scheme(listener, routes, scheme).await;
        });
        format!("http://{addr}")
    }

    fn routes_to(upstream: std::net::SocketAddr) -> SharedRoutes {
        let mut table = RoutingTable::new();
        table.insert(
            "backend.dev.example.com",
            Route {
                upstream,
                state: RouteState::Ready,
            },
        );
        SharedRoutes::new(table)
    }

    /// The plain listener (`Scheme::Http`, what `serve()` always uses) must
    /// tag forwarded requests `x-forwarded-proto: http`. This is the
    /// existing, pre-TLS behaviour and must stay byte-identical.
    #[tokio::test]
    async fn plain_scheme_forwards_proto_http() {
        let (upstream_addr, seen) = spawn_proto_capturing_upstream().await;
        let base = spawn_proxy_with_scheme(routes_to(upstream_addr), Scheme::Http).await;

        reqwest::Client::new()
            .get(&base)
            .header("host", "backend.dev.example.com")
            .send()
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some("http"),
            "the plain listener must tag requests as http"
        );
    }

    /// The TLS listener passes `Scheme::Https` into the exact same `handle`
    /// used above; a request through it must be tagged
    /// `x-forwarded-proto: https` — a genuinely different value than the
    /// plain-scheme test above, not merely "a value is present".
    #[tokio::test]
    async fn https_scheme_forwards_proto_https() {
        let (upstream_addr, seen) = spawn_proto_capturing_upstream().await;
        let base = spawn_proxy_with_scheme(routes_to(upstream_addr), Scheme::Https).await;

        reqwest::Client::new()
            .get(&base)
            .header("host", "backend.dev.example.com")
            .send()
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some("https"),
            "a connection tagged Scheme::Https must forward https, not http"
        );
    }
}
