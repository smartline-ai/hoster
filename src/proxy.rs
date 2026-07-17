use std::convert::Infallible;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderValue, HOST};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;

use crate::routing::{RouteState, SharedRoutes};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Response body type. Upstream bodies stream through; error pages are `Full`.
pub type ProxyBody = BoxBody<Bytes, BoxError>;

/// Client used to reach containers. Plain HTTP: TLS terminates at the proxy.
pub type HttpClient = Client<HttpConnector, Incoming>;

pub fn build_client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build_http()
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

/// Accept loop. Runs until the process ends.
pub async fn serve(listener: TcpListener, routes: SharedRoutes) -> anyhow::Result<()> {
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

        let routes = routes.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| handle(req, routes.clone(), client.clone()));
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

    // The app needs to know the name the browser used — it generates absolute
    // URLs from it. The original Host header is left intact; these are the
    // conventional extras.
    if let Ok(v) = HeaderValue::from_str(&host) {
        req.headers_mut().insert("x-forwarded-host", v);
    }
    req.headers_mut()
        .insert("x-forwarded-proto", HeaderValue::from_static("http"));

    match client.request(req).await {
        Ok(resp) => Ok(resp.map(|b| b.map_err(BoxError::from).boxed())),
        Err(e) => {
            tracing::warn!(%host, upstream = %route.upstream, error = %e, "upstream failed");
            Ok(text(StatusCode::BAD_GATEWAY, "upstream unavailable"))
        }
    }
}
