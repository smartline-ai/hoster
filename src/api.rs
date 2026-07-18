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
        (Method::PUT, p) if parse_registry_path(p).is_some() => {
            let project = parse_registry_path(p).unwrap();
            handle_set_registry(req, &engine, project).await
        }
        (Method::DELETE, p) if parse_registry_path(p).is_some() => {
            let project = parse_registry_path(p).unwrap();
            Ok(handle_delete_registry(&engine, &project))
        }
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

/// Extract `<project>` from `/projects/<project>/registry`, or `None` if the
/// path isn't that shape.
fn parse_registry_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/projects/")?;
    let project = rest.strip_suffix("/registry")?;
    if project.is_empty() || project.contains('/') {
        return None;
    }
    Some(project.to_string())
}

/// The body of `PUT /projects/<project>/vars/<key>`.
#[derive(Debug, Deserialize)]
struct SetVarBody {
    value: String,
    #[serde(default)]
    services: Vec<String>,
}

/// The body of `PUT /projects/<project>/registry`.
#[derive(Debug, Deserialize)]
struct SetRegistryBody {
    registry: String,
    username: String,
    password: String,
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

async fn handle_set_registry<R: ContainerRuntime>(
    req: Request<Incoming>,
    engine: &Engine<R>,
    project: String,
) -> Result<Response<ApiBody>, Infallible> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(text(StatusCode::BAD_REQUEST, "could not read request body")),
    };
    let body: SetRegistryBody = match serde_json::from_slice(&bytes) {
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
        .set_registry(&project, &body.registry, &body.username, &body.password)
    {
        Ok(()) => Ok(text(StatusCode::NO_CONTENT, "")),
        Err(msg) => Ok(text_owned(StatusCode::BAD_REQUEST, msg)),
    }
}

fn handle_delete_registry<R: ContainerRuntime>(
    engine: &Engine<R>,
    project: &str,
) -> Response<ApiBody> {
    let _ = engine.store().delete_registry(project);
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
///   `<project>/registry`          — set/replace the registry credential (form body)
///   `<project>/registry/delete`   — remove the registry credential
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

    // Must be checked before the generic "/delete" block below:
    // `strip_suffix("/registry")` would not match "<project>/registry/delete",
    // so without this block first the generic "/delete" handler would catch
    // it and mis-route into `delete_project`, wiping every stored variable
    // for the project instead of just the credential.
    if let Some(project) = sub.strip_suffix("/registry/delete") {
        let _ = engine.store().delete_registry(project);
        return redirect("/");
    }

    if let Some(project) = sub.strip_suffix("/registry") {
        let project = project.to_string();
        let bytes = match req.into_body().collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return text(StatusCode::BAD_REQUEST, "could not read request body"),
        };
        let registry = form_field(&bytes, "registry").unwrap_or_default();
        let username = form_field(&bytes, "username").unwrap_or_default();
        let password = form_field(&bytes, "password").unwrap_or_default();
        return match engine
            .store()
            .set_registry(&project, &registry, &username, &password)
        {
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

// --- Unit tests for the bearer-token API routes ---
//
// These drive `handle_api` directly over an in-memory duplex connection (no
// real TCP listener), which keeps them fast while still exercising the real
// hyper request/response path end to end. `tests/api.rs` covers the same
// surface at the integration level (real socket + reqwest); this module
// exists for handler-level unit coverage such as the registry endpoints.
#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::io::duplex;

    use super::*;
    use crate::engine::{AlwaysReady, Engine};
    use crate::routing::{RoutingTable, SharedRoutes};
    use crate::runtime::FakeRuntime;
    use crate::secrets::Store;
    use crate::session::Sessions;

    /// A unique, non-existent store path per test, so tests never share state.
    fn temp_store() -> Arc<Store> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "hoster-api-unit-{}-{n}/projects.json",
            std::process::id()
        ));
        Arc::new(Store::load(path).unwrap())
    }

    /// A fresh engine, settings, and session table for one test.
    fn api_harness() -> (Arc<Engine<FakeRuntime>>, Arc<Settings>, Arc<Sessions>) {
        let rt = Arc::new(FakeRuntime::new());
        let settings = Arc::new(Settings {
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "reg.example.com".into(),
            token: "secret".into(),
            dashboard_password: None,
        });
        let engine = Arc::new(Engine::with_readiness(
            rt,
            SharedRoutes::new(RoutingTable::new()),
            settings.clone(),
            Arc::new(AlwaysReady),
            temp_store(),
        ));
        let sessions = Arc::new(Sessions::new());
        (engine, settings, sessions)
    }

    /// Drive `handle_api` over an in-memory duplex connection and return its
    /// response, with or without the bearer token attached.
    async fn call_with_auth(
        engine: &Arc<Engine<FakeRuntime>>,
        settings: &Arc<Settings>,
        sessions: &Arc<Sessions>,
        method: Method,
        path: &str,
        body: &str,
        token: Option<&str>,
    ) -> Response<Incoming> {
        let (client_io, server_io) = duplex(64 * 1024);

        let engine = engine.clone();
        let settings_for_server = settings.clone();
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                handle_api(
                    req,
                    engine.clone(),
                    settings_for_server.clone(),
                    sessions.clone(),
                )
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(server_io), service)
                .await;
        });

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client_io))
            .await
            .expect("client handshake failed");
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        if let Some(t) = token {
            builder = builder.header(AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = builder
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("request is always valid");

        sender.send_request(req).await.expect("request failed")
    }

    async fn call(
        engine: &Arc<Engine<FakeRuntime>>,
        settings: &Arc<Settings>,
        sessions: &Arc<Sessions>,
        method: Method,
        path: &str,
        body: &str,
    ) -> Response<Incoming> {
        let token = settings.token.clone();
        call_with_auth(engine, settings, sessions, method, path, body, Some(&token)).await
    }

    async fn call_without_token(
        engine: &Arc<Engine<FakeRuntime>>,
        settings: &Arc<Settings>,
        sessions: &Arc<Sessions>,
        method: Method,
        path: &str,
        body: &str,
    ) -> Response<Incoming> {
        call_with_auth(engine, settings, sessions, method, path, body, None).await
    }

    /// A fresh engine/settings/sessions trio with the dashboard enabled, plus
    /// a `Cookie` header value for an already-valid session — for exercising
    /// the cookie-authenticated `/ui/*` routes.
    fn dashboard_harness() -> (
        Arc<Engine<FakeRuntime>>,
        Arc<Settings>,
        Arc<Sessions>,
        String,
    ) {
        let rt = Arc::new(FakeRuntime::new());
        let settings = Arc::new(Settings {
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "reg.example.com".into(),
            token: "secret".into(),
            dashboard_password: Some("dashpw".into()),
        });
        let engine = Arc::new(Engine::with_readiness(
            rt,
            SharedRoutes::new(RoutingTable::new()),
            settings.clone(),
            Arc::new(AlwaysReady),
            temp_store(),
        ));
        let sessions = Arc::new(Sessions::new());
        let cookie = format!("{SESSION_COOKIE}={}", sessions.create());
        (engine, settings, sessions, cookie)
    }

    /// Drive `handle_api` over an in-memory duplex connection with a session
    /// `Cookie` header rather than a bearer token, for the `/ui/*` routes.
    async fn call_with_cookie(
        engine: &Arc<Engine<FakeRuntime>>,
        settings: &Arc<Settings>,
        sessions: &Arc<Sessions>,
        method: Method,
        path: &str,
        body: &str,
        cookie: &str,
    ) -> Response<Incoming> {
        let (client_io, server_io) = duplex(64 * 1024);

        let engine = engine.clone();
        let settings_for_server = settings.clone();
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| {
                handle_api(
                    req,
                    engine.clone(),
                    settings_for_server.clone(),
                    sessions.clone(),
                )
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(server_io), service)
                .await;
        });

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(client_io))
            .await
            .expect("client handshake failed");
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header(COOKIE, cookie)
            .body(Full::new(Bytes::from(body.to_string())))
            .expect("request is always valid");

        sender.send_request(req).await.expect("request failed")
    }

    async fn body_string(res: Response<Incoming>) -> String {
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn put_registry_stores_the_credential() {
        let (engine, settings, sessions) = api_harness();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/registry",
            r#"{"registry":"ghcr.io","username":"bot","password":"ghp_secret"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let c = engine.store().registry_for("myproj").unwrap();
        assert_eq!(c.registry, "ghcr.io");
        assert_eq!(c.password, "ghp_secret");
    }

    #[tokio::test]
    async fn get_projects_masks_the_registry_password() {
        let (engine, settings, sessions) = api_harness();
        engine
            .store()
            .set_registry("myproj", "ghcr.io", "bot", "ghp_topsecret")
            .unwrap();
        let res = call(&engine, &settings, &sessions, Method::GET, "/projects", "").await;
        let body = body_string(res).await;
        assert!(!body.contains("ghp_topsecret"), "password leaked: {body}");
        assert!(body.contains("ghcr.io"));
        assert!(body.contains("bot"));
    }

    #[tokio::test]
    async fn delete_registry_removes_the_credential() {
        let (engine, settings, sessions) = api_harness();
        engine
            .store()
            .set_registry("myproj", "ghcr.io", "bot", "x")
            .unwrap();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::DELETE,
            "/projects/myproj/registry",
            "",
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(engine.store().registry_for("myproj").is_none());
    }

    #[tokio::test]
    async fn put_registry_rejects_an_empty_username() {
        let (engine, settings, sessions) = api_harness();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/registry",
            r#"{"registry":"ghcr.io","username":"","password":"x"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn registry_endpoints_require_the_bearer_token() {
        let (engine, settings, sessions) = api_harness();
        let res = call_without_token(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/registry",
            r#"{"registry":"ghcr.io","username":"bot","password":"x"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ui_projects_registry_sets_the_credential() {
        let (engine, settings, sessions, cookie) = dashboard_harness();
        let res = call_with_cookie(
            &engine,
            &settings,
            &sessions,
            Method::POST,
            "/ui/projects/myproj/registry",
            "registry=ghcr.io&username=bot&password=ghp_secret",
            &cookie,
        )
        .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        let c = engine.store().registry_for("myproj").unwrap();
        assert_eq!(c.registry, "ghcr.io");
        assert_eq!(c.username, "bot");
        assert_eq!(c.password, "ghp_secret");
    }

    #[tokio::test]
    async fn ui_projects_registry_delete_removes_the_credential() {
        let (engine, settings, sessions, cookie) = dashboard_harness();
        engine
            .store()
            .set_registry("myproj", "ghcr.io", "bot", "x")
            .unwrap();
        let res = call_with_cookie(
            &engine,
            &settings,
            &sessions,
            Method::POST,
            "/ui/projects/myproj/registry/delete",
            "",
            &cookie,
        )
        .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(engine.store().registry_for("myproj").is_none());
    }

    /// Regression guard for the route-ordering requirement: `/registry/delete`
    /// must be matched before the generic `/delete` suffix (which routes to
    /// `delete_project` and would wipe every stored variable), and before
    /// `/registry` (whose `strip_suffix` wouldn't match the longer path
    /// anyway, but the ordering is what keeps it that way on purpose).
    #[tokio::test]
    async fn ui_projects_registry_delete_does_not_wipe_the_projects_vars() {
        let (engine, settings, sessions, cookie) = dashboard_harness();
        engine
            .store()
            .set_var("myproj", "KEEP_ME", "v", vec![])
            .unwrap();
        engine
            .store()
            .set_registry("myproj", "ghcr.io", "bot", "x")
            .unwrap();
        let res = call_with_cookie(
            &engine,
            &settings,
            &sessions,
            Method::POST,
            "/ui/projects/myproj/registry/delete",
            "",
            &cookie,
        )
        .await;
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert!(engine.store().registry_for("myproj").is_none());
        assert_eq!(
            engine.store().env_for("myproj", "any").get("KEEP_ME"),
            Some(&"v".to_string())
        );
    }
}
