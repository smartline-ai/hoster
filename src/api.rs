//! The control API: a shared-token-authenticated HTTP interface for
//! deploying, tearing down, and listing branch environments.
//!
//! This runs on its own listener (`settings.api_listen`), separate from the
//! proxy's listener. It is served publicly (behind TLS at
//! hoster.odinvestor.net) and every bearer route is guarded by the shared
//! token; the cookie-authenticated UI routes below have their own, separate
//! guard.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{AUTHORIZATION, COOKIE, LOCATION, SET_COOKIE};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::config::{self, DeployConfig};
use crate::dashboard;
use crate::engine::{DeployRequest, Engine};
use crate::runtime::ContainerRuntime;
use crate::session::{Sessions, constant_time_eq, cookie_value};
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

/// Bearer-token check against a publicly reachable listener — compared in
/// constant time so response timing can't leak how much of the token a
/// guess got right.
fn is_authorized(req: &Request<Incoming>, settings: &Settings) -> bool {
    match req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(got) => {
            let expected = format!("Bearer {}", settings.token);
            constant_time_eq(got.as_bytes(), expected.as_bytes())
        }
        None => false,
    }
}

/// Accept loop for the control API. Runs until the process ends. Mirrors
/// `proxy::serve`'s shape: one hyper http1 connection per accepted socket.
pub async fn serve_api<R: ContainerRuntime + 'static>(
    listener: TcpListener,
    engine: Arc<Engine<R>>,
    settings: Arc<Settings>,
) -> anyhow::Result<()> {
    tracing::info!(addr = %listener.local_addr()?, "api listening");
    let sessions = Arc::new(Sessions::new());

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
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                handle_api(req, engine.clone(), settings.clone(), sessions.clone())
            });
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
    sessions: Arc<Sessions>,
) -> Result<Response<ApiBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // /healthz is the one route open to unauthenticated callers (load
    // balancer / orchestrator health checks shouldn't need the token).
    if method == Method::GET && path == "/healthz" {
        return Ok(text(StatusCode::OK, "ok"));
    }

    // --- UI routes (cookie auth), matched before the bearer gate ---
    match (&method, path.as_str()) {
        (&Method::GET, "/") => return Ok(ui_root(&req, &engine, &settings, &sessions).await),
        (&Method::GET, "/login") => return Ok(ui_login_page(&settings, None)),
        (&Method::POST, "/login") => return Ok(ui_login_submit(req, &settings, &sessions).await),
        (&Method::POST, "/logout") => return Ok(ui_logout(&req, &settings, &sessions)),
        (&Method::POST, p) if p.starts_with("/ui/destroy/") => {
            let branch = p.trim_start_matches("/ui/destroy/").to_string();
            return Ok(ui_destroy(&req, engine, &settings, &sessions, branch).await);
        }
        (&Method::POST, p) if p.starts_with("/ui/projects/") => {
            let sub = p.trim_start_matches("/ui/projects/").to_string();
            return Ok(ui_projects(req, engine, &settings, &sessions, sub).await);
        }
        _ => {}
    }

    // --- bearer-token API routes ---
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
        (Method::GET, "/projects") => Ok(handle_list_projects(&engine)),
        (Method::PUT, p) if parse_var_path(p).is_some() => {
            let (project, key) = parse_var_path(p).unwrap();
            handle_set_var(req, &engine, project, key).await
        }
        (Method::DELETE, p) if parse_var_path(p).is_some() => {
            let (project, key) = parse_var_path(p).unwrap();
            Ok(handle_delete_var(&engine, &project, &key))
        }
        _ => Ok(text(StatusCode::NOT_FOUND, "not found")),
    }
}

/// Split `/projects/<project>/vars/<key>` into its two segments, or `None` if
/// the path isn't that shape or a segment is empty.
fn parse_var_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/projects/")?;
    let (project, key) = rest.split_once("/vars/")?;
    if project.is_empty() || key.is_empty() || key.contains('/') {
        return None;
    }
    Some((project.to_string(), key.to_string()))
}

/// The body of `PUT /projects/<project>/vars/<key>`.
#[derive(Debug, Deserialize)]
struct SetVarBody {
    value: String,
    #[serde(default)]
    services: Vec<String>,
}

fn handle_list_projects<R: ContainerRuntime>(engine: &Engine<R>) -> Response<ApiBody> {
    let masked = engine.store().list_masked();
    let bytes = serde_json::to_vec(&masked).unwrap_or_default();
    json_bytes(StatusCode::OK, bytes)
}

async fn handle_set_var<R: ContainerRuntime>(
    req: Request<Incoming>,
    engine: &Engine<R>,
    project: String,
    key: String,
) -> Result<Response<ApiBody>, Infallible> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(text(StatusCode::BAD_REQUEST, "could not read request body")),
    };
    let body: SetVarBody = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(text_owned(
                StatusCode::BAD_REQUEST,
                format!("invalid request body: {e}"),
            ));
        }
    };
    match engine
        .store()
        .set_var(&project, &key, &body.value, body.services)
    {
        Ok(()) => Ok(text(StatusCode::NO_CONTENT, "")),
        Err(msg) => Ok(text_owned(StatusCode::BAD_REQUEST, msg)),
    }
}

fn handle_delete_var<R: ContainerRuntime>(
    engine: &Engine<R>,
    project: &str,
    key: &str,
) -> Response<ApiBody> {
    let _ = engine.store().delete_var(project, key);
    text(StatusCode::NO_CONTENT, "")
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

// --- Cookie-authenticated dashboard UI routes ---
//
// These serve human operators a browser session (login form + deployment
// table) that is entirely separate from the bearer-token API above: they
// authenticate via a `hoster_session` cookie, never the `Authorization`
// header, and are matched in `handle_api` before the bearer gate runs.

const SESSION_COOKIE: &str = "hoster_session";

fn html(status: StatusCode, body: String) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("html response is always valid")
}

fn redirect(location: &str) -> Response<ApiBody> {
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(LOCATION, location)
        .body(Full::new(Bytes::new()))
        .expect("redirect is always valid")
}

/// Whether the request carries a valid session cookie.
fn session_of(req: &Request<Incoming>, sessions: &Sessions) -> bool {
    let raw = req.headers().get(COOKIE).and_then(|v| v.to_str().ok());
    cookie_value(raw, SESSION_COOKIE)
        .map(|t| sessions.validate(&t))
        .unwrap_or(false)
}

/// The dashboard password, or None if unset OR empty. An empty password never
/// enables the dashboard (an empty form field would otherwise match it).
fn dashboard_password(settings: &Settings) -> Option<&str> {
    settings
        .dashboard_password
        .as_deref()
        .filter(|p| !p.is_empty())
}

/// None → the dashboard is not configured; every UI route answers 503.
fn dashboard_enabled(settings: &Settings) -> bool {
    dashboard_password(settings).is_some()
}

fn ui_login_page(settings: &Settings, error: Option<&str>) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    html(StatusCode::OK, dashboard::login_page(error))
}

async fn ui_login_submit(
    req: Request<Incoming>,
    settings: &Settings,
    sessions: &Sessions,
) -> Response<ApiBody> {
    let Some(expected) = dashboard_password(settings) else {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    };
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return html(
                StatusCode::BAD_REQUEST,
                dashboard::login_page(Some("Bad request")),
            );
        }
    };
    // form body: password=...
    let submitted = form_field(&bytes, "password").unwrap_or_default();
    if constant_time_eq(submitted.as_bytes(), expected.as_bytes()) {
        let token = sessions.create();
        let cookie = format!(
            "{SESSION_COOKIE}={token}; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=86400"
        );
        return Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(LOCATION, "/")
            .header(SET_COOKIE, cookie)
            .body(Full::new(Bytes::new()))
            .expect("login redirect is always valid");
    }
    html(
        StatusCode::OK,
        dashboard::login_page(Some("Invalid password")),
    )
}

fn ui_logout(
    req: &Request<Incoming>,
    settings: &Settings,
    sessions: &Sessions,
) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    if let Some(tok) = req
        .headers()
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(Some(c), SESSION_COOKIE))
    {
        sessions.remove(&tok);
    }
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(LOCATION, "/login")
        .header(
            SET_COOKIE,
            format!("{SESSION_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0"),
        )
        .body(Full::new(Bytes::new()))
        .expect("logout redirect is always valid")
}

async fn ui_root<R: ContainerRuntime>(
    req: &Request<Incoming>,
    engine: &Engine<R>,
    settings: &Settings,
    sessions: &Sessions,
) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    if !session_of(req, sessions) {
        return redirect("/login");
    }
    let deployments = engine.deployment_views().await.unwrap_or_default();
    let env = engine.store().list_masked();
    html(
        StatusCode::OK,
        dashboard::dashboard_page(&deployments, &env),
    )
}

/// The dashboard's env-management POST routes, all cookie-authenticated:
///   `<project>/vars`              — set/replace a variable (form body)
///   `<project>/vars/<key>/delete` — delete a variable
///   `<project>/delete`            — delete all of a project's variables
async fn ui_projects<R: ContainerRuntime>(
    req: Request<Incoming>,
    engine: Arc<Engine<R>>,
    settings: &Settings,
    sessions: &Sessions,
    sub: String,
) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    if !session_of(&req, sessions) {
        return redirect("/login");
    }

    if let Some(project) = sub.strip_suffix("/vars") {
        let project = project.to_string();
        let bytes = match req.into_body().collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return text(StatusCode::BAD_REQUEST, "could not read request body"),
        };
        let key = form_field(&bytes, "key").unwrap_or_default();
        let value = form_field(&bytes, "value").unwrap_or_default();
        let services = form_field(&bytes, "services")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        return match engine.store().set_var(&project, &key, &value, services) {
            Ok(()) => redirect("/"),
            Err(msg) => text_owned(StatusCode::BAD_REQUEST, msg),
        };
    }

    if let Some(head) = sub.strip_suffix("/delete") {
        if let Some((project, key)) = head.split_once("/vars/") {
            let _ = engine.store().delete_var(project, key);
        } else {
            let _ = engine.store().delete_project(head);
        }
        return redirect("/");
    }

    text(StatusCode::NOT_FOUND, "not found")
}

async fn ui_destroy<R: ContainerRuntime>(
    req: &Request<Incoming>,
    engine: Arc<Engine<R>>,
    settings: &Settings,
    sessions: &Sessions,
    branch: String,
) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    if !session_of(req, sessions) {
        return redirect("/login");
    }
    let _ = engine.teardown(&branch).await;
    redirect("/")
}

/// Minimal `application/x-www-form-urlencoded` field extractor — enough for the
/// login form's single `password` field. Handles `+` and `%XX` decoding.
fn form_field(body: &[u8], name: &str) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    for pair in s.split('&') {
        if let Some((k, v)) = pair.split_once('=')
            && k == name
        {
            return Some(url_decode(v));
        }
    }
    None
}

fn url_decode(s: &str) -> String {
    let bytes = s.replace('+', " ");
    let bytes = bytes.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
