//! The control API: a shared-token-authenticated HTTP interface for
//! deploying, tearing down, and listing branch environments.
//!
//! This runs on its own listener (`settings.api_listen`), separate from the
//! proxy's listener, and is never entered into the routing table — it is not
//! meant to be reachable from outside the operator's network.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::AUTHORIZATION;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::config::{self, DeployConfig};
use crate::engine::{DeployRequest, Engine};
use crate::runtime::ContainerRuntime;
use crate::settings::{Settings, sanitize_branch};

/// Response body type. Every response this API produces is small enough to
/// buffer whole, so there is no need for the boxed streaming body the proxy
/// uses.
pub type ApiBody = Full<Bytes>;

/// The `POST /deploy` request shape. `config` reuses `DeployConfig`'s own
/// `Deserialize` impl (and its `deny_unknown_fields`), so malformed configs
/// are rejected the same way whether they arrive over the API or in tests.
#[derive(Debug, Deserialize)]
struct DeployBody {
    branch: String,
    tag: String,
    sha: String,
    config: DeployConfig,
}

fn text(status: StatusCode, body: &'static str) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("static response is always valid")
}

fn text_owned(status: StatusCode, body: String) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("static response is always valid")
}

fn json_bytes(status: StatusCode, bytes: Vec<u8>) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .expect("static response is always valid")
}

/// A single shared secret over a trusted, unrouted port — a plain
/// byte-for-byte comparison is an acceptable and simplest-possible check
/// here, no `subtle`-style constant-time compare needed.
fn is_authorized(req: &Request<Incoming>, settings: &Settings) -> bool {
    let expected = format!("Bearer {}", settings.token);
    req.headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false)
}

/// Accept loop for the control API. Runs until the process ends. Mirrors
/// `proxy::serve`'s shape: one hyper http1 connection per accepted socket.
pub async fn serve_api<R: ContainerRuntime + 'static>(
    listener: TcpListener,
    engine: Arc<Engine<R>>,
    settings: Arc<Settings>,
) -> anyhow::Result<()> {
    tracing::info!(addr = %listener.local_addr()?, "api listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // Per-connection accept errors must never kill the listener.
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };

        let engine = engine.clone();
        let settings = settings.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| handle_api(req, engine.clone(), settings.clone()));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!(%peer, error = %e, "connection closed with error");
            }
        });
    }
}

/// The whole request path: authenticate, route, respond.
///
/// Returns `Infallible` because every failure is a response, same discipline
/// as `proxy::handle` — an `Err` returned to hyper just drops the connection.
pub async fn handle_api<R: ContainerRuntime + 'static>(
    req: Request<Incoming>,
    engine: Arc<Engine<R>>,
    settings: Arc<Settings>,
) -> Result<Response<ApiBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // /healthz is the one route open to unauthenticated callers (load
    // balancer / orchestrator health checks shouldn't need the token).
    if method == Method::GET && path == "/healthz" {
        return Ok(text(StatusCode::OK, "ok"));
    }

    if !is_authorized(&req, &settings) {
        return Ok(text(StatusCode::UNAUTHORIZED, "unauthorized"));
    }

    match (method, path.as_str()) {
        (Method::POST, "/deploy") => handle_deploy(req, engine).await,
        (Method::GET, "/deployments") => Ok(handle_deployments(&engine)),
        (Method::DELETE, p) if p.starts_with("/deploy/") => {
            let branch = p.trim_start_matches("/deploy/").to_string();
            Ok(handle_teardown(engine, branch).await)
        }
        _ => Ok(text(StatusCode::NOT_FOUND, "not found")),
    }
}

/// Validate synchronously, then hand the actual provisioning to a background
/// task — deploys can take seconds (image pulls, container starts) and the
/// caller shouldn't block the connection on that.
async fn handle_deploy<R: ContainerRuntime + 'static>(
    req: Request<Incoming>,
    engine: Arc<Engine<R>>,
) -> Result<Response<ApiBody>, Infallible> {
    let bytes = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) => {
            tracing::warn!(error = %e, "failed to read request body");
            return Ok(text(StatusCode::BAD_REQUEST, "could not read request body"));
        }
    };

    let body: DeployBody = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(text_owned(
                StatusCode::BAD_REQUEST,
                format!("invalid request body: {e}"),
            ));
        }
    };

    if let Err(msg) = config::validate(&body.config) {
        return Ok(text_owned(StatusCode::BAD_REQUEST, msg));
    }

    let deploy_req = DeployRequest {
        branch: body.branch,
        tag: body.tag,
        sha: body.sha,
        config: body.config,
    };

    // Computed the same way `deploy` computes them, so the caller learns the
    // final URLs immediately without waiting for provisioning to finish.
    let urls = engine.plan_urls(&deploy_req);
    let branch = sanitize_branch(&deploy_req.branch);

    let eng = engine.clone();
    tokio::spawn(async move {
        let _ = eng.deploy(deploy_req).await;
    });

    let payload = serde_json::json!({ "branch": branch, "urls": urls });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    Ok(json_bytes(StatusCode::ACCEPTED, bytes))
}

fn handle_deployments<R: ContainerRuntime>(engine: &Engine<R>) -> Response<ApiBody> {
    let list = engine.deployments();
    let bytes = serde_json::to_vec(&list).unwrap_or_default();
    json_bytes(StatusCode::OK, bytes)
}

/// Teardown is idempotent: a branch that doesn't exist is not an error, so
/// this always answers `204` regardless of what `teardown` returns.
async fn handle_teardown<R: ContainerRuntime>(
    engine: Arc<Engine<R>>,
    branch: String,
) -> Response<ApiBody> {
    let _ = engine.teardown(&branch).await;
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Full::new(Bytes::new()))
        .expect("static response is always valid")
}
