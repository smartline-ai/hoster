# Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A server-rendered status dashboard at `hoster.odinvestor.net/` — login with a shared password, see the branch deployments, destroy one. Cookie auth for the UI; the bearer-token API is unchanged.

**Architecture:** A pure HTML-rendering module (`dashboard.rs`) plus a session store + cookie helpers (`session.rs`), wired into the existing control API (`api.rs`). The UI routes are matched before the bearer-token gate and authenticate by session cookie instead.

**Tech Stack:** Rust 2024, `hyper` 1.x (already the API server), `getrandom` (session tokens), `serde`. No web framework, no JS framework.

## Global Constraints

From `docs/superpowers/specs/2026-07-18-dashboard-design.md`.

- **Two doors, no overlap.** Bearer-token routes (`/deploy`, `/deployments`, `/healthz`) keep bearer auth and never accept the cookie. UI routes (`/`, `/login`, `/logout`, `/ui/destroy/*`) use the session cookie and never accept the bearer token.
- **Additive.** If `HOSTER_DASHBOARD_PASSWORD` is unset, the UI routes return "dashboard not configured" and the API behaves exactly as today. Existing API integration tests must still pass unchanged.
- **Every value interpolated into HTML is escaped.** Statuses carry free text (`failed: <message>`); an unescaped `<script>` is an XSS bug. Tested.
- **Sessions are in-memory**, process-lived, no DB. Consistent with the labels-not-database stance.
- **The dashboard can destroy but never deploy** — no config is ever accepted through the UI.
- `dashboard.rs` is pure (data → String, no hyper/IO). `session.rs` has no hyper types. HTTP lives only in `api.rs`.
- Cookie flags: `HttpOnly; Secure; SameSite=Lax; Path=/`.

## Interfaces produced

```rust
// dashboard.rs (Task 1)
pub fn html_escape(s: &str) -> String;
pub fn login_page(error: Option<&str>) -> String;
pub fn dashboard_page(deployments: &[crate::engine::DeploymentInfo]) -> String;

// session.rs (Task 2)
pub struct Sessions { /* Mutex<HashSet<String>> */ }
impl Sessions { pub fn new() -> Self; pub fn create(&self) -> String; pub fn validate(&self, token: &str) -> bool; pub fn remove(&self, token: &str); }
pub fn cookie_value(header: Option<&str>, name: &str) -> Option<String>;
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool;

// settings.rs (Task 2): Settings gains `pub dashboard_password: Option<String>`
```

## File Structure

Create: `src/dashboard.rs`, `src/session.rs`, `tests/dashboard.rs`. Modify: `src/api.rs`, `src/settings.rs`, `src/main.rs`, `src/lib.rs`, `Cargo.toml`.

---

### Task 1: Pure HTML rendering (`dashboard.rs`)

**Files:** Create `src/dashboard.rs`; Modify `src/lib.rs`.

**Consumes:** `crate::engine::DeploymentInfo` (`{ branch: String, status: String, urls: BTreeMap<String,String> }`).

- [ ] **Step 1: Write failing tests**

Create `src/dashboard.rs`:

```rust
use std::fmt::Write;

use crate::engine::DeploymentInfo;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dep(branch: &str, status: &str, urls: &[(&str, &str)]) -> DeploymentInfo {
        DeploymentInfo {
            branch: branch.to_string(),
            status: status.to_string(),
            urls: urls.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn escapes_html() {
        assert_eq!(html_escape("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
    }

    #[test]
    fn login_page_has_password_form() {
        let html = login_page(None);
        assert!(html.contains("<form"));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/login\""));
        assert!(html.contains("type=\"password\""));
        assert!(!html.contains("Invalid"));
    }

    #[test]
    fn login_page_shows_error() {
        let html = login_page(Some("Invalid password"));
        assert!(html.contains("Invalid password"));
    }

    #[test]
    fn dashboard_lists_a_deployment() {
        let html = dashboard_page(&[dep("feature-x", "running", &[("backend", "http://backend-feature-x.example.com")])]);
        assert!(html.contains("feature-x"));
        assert!(html.contains("running"));
        assert!(html.contains("http://backend-feature-x.example.com"));
        // a destroy form pointing at the branch
        assert!(html.contains("/ui/destroy/feature-x"));
    }

    #[test]
    fn dashboard_escapes_status_text() {
        // A failed status carrying an injection payload must be escaped.
        let html = dashboard_page(&[dep("b", "failed: <script>alert(1)</script>", &[])]);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn dashboard_empty_state() {
        let html = dashboard_page(&[]);
        assert!(html.to_lowercase().contains("no deployments"));
    }
}
```

Add `pub mod dashboard;` to `src/lib.rs` (keep the others).

- [ ] **Step 2: Run tests, expect failure**

Run: `cargo test --lib dashboard`
Expected: compile failure — functions not found.

- [ ] **Step 3: Implement**

Add above the tests in `src/dashboard.rs`:

```rust
/// Escape the five HTML-significant characters. Applied to every dynamic value
/// rendered into a page — statuses in particular carry arbitrary error text.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const STYLE: &str = "\
body{font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;color:#1a1a1a}\
h1{font-size:1.4rem}table{width:100%;border-collapse:collapse;margin-top:1rem}\
th,td{text-align:left;padding:.5rem .6rem;border-bottom:1px solid #e2e2e2;vertical-align:top}\
th{font-size:.8rem;text-transform:uppercase;color:#666}\
.status{font-weight:600}.status.running{color:#137333}.status.failed{color:#c5221f}.status.provisioning{color:#b06000}\
a{color:#1558d6}button{cursor:pointer}\
.destroy{background:#c5221f;color:#fff;border:0;padding:.35rem .7rem;border-radius:4px}\
form.login{display:flex;gap:.5rem;margin-top:1rem}input{padding:.5rem;border:1px solid #ccc;border-radius:4px}\
.err{color:#c5221f;margin-top:.5rem}.empty{color:#666;margin-top:1rem}\
@media(prefers-color-scheme:dark){body{background:#111;color:#eee}th{color:#aaa}td,th{border-color:#333}}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        html_escape(title)
    )
}

/// The login form. `error` renders a message above the form when a prior
/// attempt failed.
pub fn login_page(error: Option<&str>) -> String {
    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>hoster</h1>{err}\
<form class=\"login\" method=\"post\" action=\"/login\">\
<input type=\"password\" name=\"password\" placeholder=\"Password\" autofocus>\
<button type=\"submit\">Sign in</button></form>"
    );
    page("hoster — sign in", &body)
}

/// The deployment list, one row per branch, each with its URLs and a destroy
/// button.
pub fn dashboard_page(deployments: &[DeploymentInfo]) -> String {
    let mut body = String::new();
    let _ = write!(
        body,
        "<h1>hoster</h1><form method=\"post\" action=\"/logout\" style=\"float:right\">\
<button type=\"submit\">Sign out</button></form>"
    );

    if deployments.is_empty() {
        body.push_str("<p class=\"empty\">No deployments yet.</p>");
        return page("hoster — dashboard", &body);
    }

    body.push_str("<table><thead><tr><th>Branch</th><th>Status</th><th>URLs</th><th></th></tr></thead><tbody>");
    for d in deployments {
        let branch = html_escape(&d.branch);
        let status_class = d.status.split(':').next().unwrap_or("").trim();
        let links = if d.urls.is_empty() {
            "<span class=\"empty\">—</span>".to_string()
        } else {
            d.urls
                .values()
                .map(|u| {
                    let e = html_escape(u);
                    format!("<a href=\"{e}\">{e}</a>")
                })
                .collect::<Vec<_>>()
                .join("<br>")
        };
        let _ = write!(
            body,
            "<tr><td>{branch}</td>\
<td class=\"status {status_class}\">{}</td>\
<td>{links}</td>\
<td><form method=\"post\" action=\"/ui/destroy/{branch}\" \
onsubmit=\"return confirm('Destroy {branch}?')\">\
<button class=\"destroy\" type=\"submit\">Destroy</button></form></td></tr>",
            html_escape(&d.status),
        );
    }
    body.push_str("</tbody></table>");
    page("hoster — dashboard", &body)
}
```

- [ ] **Step 4: Run tests, expect pass**

Run: `cargo test --lib dashboard`
Expected: 6 pass.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/dashboard.rs
git commit -m "feat: pure HTML rendering for the dashboard"
```

---

### Task 2: Sessions, cookies, and the password setting

**Files:** Modify `Cargo.toml`, `src/settings.rs`, `src/lib.rs`; Create `src/session.rs`. Also update every `Settings { ... }` literal (see Step 4).

**Interfaces produced:** `Sessions`, `cookie_value`, `constant_time_eq`, and `Settings.dashboard_password`.

- [ ] **Step 1: Add the dependency**

Add to `[dependencies]` in `Cargo.toml` (keep all existing):

```toml
getrandom = "0.2"
```

- [ ] **Step 2: Write failing tests**

Create `src/session.rs`:

```rust
use std::collections::HashSet;
use std::sync::Mutex;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_validate() {
        let s = Sessions::new();
        let t = s.create();
        assert!(s.validate(&t));
        assert!(!s.validate("nope"));
    }

    #[test]
    fn tokens_are_distinct_and_long() {
        let s = Sessions::new();
        let a = s.create();
        let b = s.create();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64); // 32 bytes hex
    }

    #[test]
    fn remove_invalidates() {
        let s = Sessions::new();
        let t = s.create();
        s.remove(&t);
        assert!(!s.validate(&t));
    }

    #[test]
    fn parses_named_cookie() {
        assert_eq!(cookie_value(Some("a=1; hoster_session=xyz; b=2"), "hoster_session"), Some("xyz".to_string()));
        assert_eq!(cookie_value(Some("hoster_session=only"), "hoster_session"), Some("only".to_string()));
        assert_eq!(cookie_value(Some("other=1"), "hoster_session"), None);
        assert_eq!(cookie_value(None, "hoster_session"), None);
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreu"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }
}
```

Add `pub mod session;` to `src/lib.rs`.

- [ ] **Step 3: Implement**

Add above the tests in `src/session.rs`:

```rust
/// In-memory session store. A session is a random 256-bit token; a hoster
/// restart clears the set (everyone re-logs-in). No persistence by design.
#[derive(Default)]
pub struct Sessions {
    tokens: Mutex<HashSet<String>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a fresh session token from the OS CSPRNG and store it.
    pub fn create(&self) -> String {
        let mut buf = [0u8; 32];
        getrandom::getrandom(&mut buf).expect("OS RNG unavailable");
        let mut token = String::with_capacity(64);
        for b in buf {
            use std::fmt::Write;
            let _ = write!(token, "{b:02x}");
        }
        self.tokens.lock().unwrap().insert(token.clone());
        token
    }

    pub fn validate(&self, token: &str) -> bool {
        self.tokens.lock().unwrap().contains(token)
    }

    pub fn remove(&self, token: &str) {
        self.tokens.lock().unwrap().remove(token);
    }
}

/// Pull one cookie's value out of a `Cookie` header (`k=v; k2=v2`).
pub fn cookie_value(header: Option<&str>, name: &str) -> Option<String> {
    let header = header?;
    for part in header.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=')
            && k == name
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Length-checked, byte-diff-accumulating comparison so a password check
/// doesn't leak how many leading bytes matched via timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
```

- [ ] **Step 4: Add `dashboard_password` to `Settings` and update every constructor**

In `src/settings.rs`, add the field to the struct:

```rust
#[derive(Debug, Clone)]
pub struct Settings {
    pub listen: String,
    pub api_listen: String,
    pub hostname_template: String,
    pub registry: String,
    pub token: String,
    pub dashboard_password: Option<String>,
}
```

Then add `dashboard_password: None` (or a value) to **every** `Settings { ... }` literal in the codebase. Find them all:

Run: `grep -rn "Settings {" src tests`

They are: `src/main.rs` (build from env — see Task 3 note), `src/engine.rs` (test helper `settings()`), and `tests/api.rs` (`spawn` helper). Add `dashboard_password: None` to the engine and api test literals now so the suite compiles; `main.rs` is updated in Task 3.

- [ ] **Step 5: Run tests, expect pass**

Run: `cargo test --lib session`
Expected: 5 pass. Also `cargo build` so the `Settings` field change compiles across the tree (fix any missed literal with `dashboard_password: None`).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/session.rs src/settings.rs src/engine.rs tests/api.rs
git commit -m "feat: session store, cookie helpers, and dashboard password setting"
```

---

### Task 3: Wire the UI routes into the control API

**Files:** Modify `src/api.rs`, `src/main.rs`; Create `tests/dashboard.rs`.

**Consumes:** everything from Tasks 1–2.

The UI routes must be handled **before** the bearer-token gate in `handle_api`, and `serve_api` owns one `Sessions` shared across all its connections.

- [ ] **Step 1: Write failing integration tests**

Create `tests/dashboard.rs`:

```rust
use std::sync::Arc;

use hoster::api::serve_api;
use hoster::engine::{AlwaysReady, Engine};
use hoster::routing::{RoutingTable, SharedRoutes};
use hoster::runtime::FakeRuntime;
use hoster::settings::Settings;
use tokio::net::TcpListener;

fn settings(password: Option<&str>) -> Arc<Settings> {
    Arc::new(Settings {
        listen: "127.0.0.1:0".into(),
        api_listen: "127.0.0.1:0".into(),
        hostname_template: "{service}-{branch}.example.com".into(),
        registry: "reg.example.com".into(),
        token: "secret".into(),
        dashboard_password: password.map(str::to_string),
    })
}

async fn spawn(password: Option<&str>) -> (String, Arc<FakeRuntime>) {
    let rt = Arc::new(FakeRuntime::new());
    let engine = Arc::new(Engine::with_readiness(
        rt.clone(),
        SharedRoutes::new(RoutingTable::new()),
        settings(password),
        Arc::new(AlwaysReady),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let s = settings(password);
    tokio::spawn(async move { serve_api(listener, engine, s).await });
    (format!("http://{addr}"), rt)
}

// A client that does NOT auto-follow redirects and DOES keep cookies.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .cookie_store(true)
        .build()
        .unwrap()
}

const DEPLOY_BODY: &str = r#"{"branch":"feature/x","tag":"t","sha":"s","config":{"project":"p","services":{"backend":{"image":"img","expose":{"port":8080}}}}}"#;

#[tokio::test]
async fn root_without_cookie_redirects_to_login() {
    let (base, _) = spawn(Some("pw")).await;
    let resp = client().get(&base).send().await.unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.headers()["location"], "/login");
}

#[tokio::test]
async fn login_wrong_password_sets_no_cookie() {
    let (base, _) = spawn(Some("pw")).await;
    let resp = client()
        .post(format!("{base}/login"))
        .form(&[("password", "wrong")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("set-cookie").is_none());
    assert!(resp.text().await.unwrap().to_lowercase().contains("invalid"));
}

#[tokio::test]
async fn login_then_dashboard_then_destroy() {
    let (base, rt) = spawn(Some("pw")).await;
    let c = client();

    // deploy a branch via the bearer API so the dashboard has a row
    c.post(format!("{base}/deploy")).bearer_auth("secret").body(DEPLOY_BODY).send().await.unwrap();
    // give the spawned deploy a moment
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(rt.container_count(), 1);

    // log in — cookie is stored by the client
    let login = c.post(format!("{base}/login")).form(&[("password", "pw")]).send().await.unwrap();
    assert_eq!(login.status(), 303);
    assert!(login.headers().get_all("set-cookie").iter().any(|v| v.to_str().unwrap().contains("hoster_session=")));

    // dashboard renders the branch
    let dash = c.get(&base).send().await.unwrap();
    assert_eq!(dash.status(), 200);
    let html = dash.text().await.unwrap();
    assert!(html.contains("feature-x"));

    // destroy it via the UI form
    let del = c.post(format!("{base}/ui/destroy/feature-x")).send().await.unwrap();
    assert_eq!(del.status(), 303);
    assert_eq!(rt.container_count(), 0);
}

#[tokio::test]
async fn destroy_without_cookie_is_rejected() {
    let (base, rt) = spawn(Some("pw")).await;
    let c = client();
    c.post(format!("{base}/deploy")).bearer_auth("secret").body(DEPLOY_BODY).send().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // no login → no cookie → destroy must not happen
    let resp = c.post(format!("{base}/ui/destroy/feature-x")).send().await.unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.headers()["location"], "/login");
    assert_eq!(rt.container_count(), 1);
}

#[tokio::test]
async fn ui_disabled_when_no_password() {
    let (base, _) = spawn(None).await;
    let resp = client().get(format!("{base}/login")).send().await.unwrap();
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn bearer_api_still_works_and_ignores_cookies() {
    let (base, _) = spawn(Some("pw")).await;
    let resp = client().get(format!("{base}/deployments")).bearer_auth("secret").send().await.unwrap();
    assert_eq!(resp.status(), 200);
    // and the cookie alone does NOT authorize the bearer route
    let no = client().get(format!("{base}/deployments")).send().await.unwrap();
    assert_eq!(no.status(), 401);
}
```

- [ ] **Step 2: Run tests, expect failure**

Run: `cargo test --test dashboard`
Expected: compile/route failures — UI routes don't exist yet.

- [ ] **Step 3: Implement the UI routes in `src/api.rs`**

Add imports at the top of `src/api.rs`:

```rust
use hyper::header::{COOKIE, LOCATION, SET_COOKIE};
use crate::dashboard;
use crate::session::{constant_time_eq, cookie_value, Sessions};
```

`serve_api` creates one `Sessions` and passes it to each connection. Change its body so the per-connection closure clones a shared `Arc<Sessions>`:

```rust
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
            Err(e) => { tracing::warn!(error = %e, "accept failed"); continue; }
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
```

Add the `sessions` parameter to `handle_api` and route the UI paths **before** the bearer gate:

```rust
pub async fn handle_api<R: ContainerRuntime + 'static>(
    req: Request<Incoming>,
    engine: Arc<Engine<R>>,
    settings: Arc<Settings>,
    sessions: Arc<Sessions>,
) -> Result<Response<ApiBody>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if method == Method::GET && path == "/healthz" {
        return Ok(text(StatusCode::OK, "ok"));
    }

    // --- UI routes (cookie auth), matched before the bearer gate ---
    match (&method, path.as_str()) {
        (&Method::GET, "/") => return Ok(ui_root(&req, &engine, &settings, &sessions)),
        (&Method::GET, "/login") => return Ok(ui_login_page(&settings, None)),
        (&Method::POST, "/login") => return Ok(ui_login_submit(req, &settings, &sessions).await),
        (&Method::POST, "/logout") => return Ok(ui_logout(&req, &sessions)),
        (&Method::POST, p) if p.starts_with("/ui/destroy/") => {
            let branch = p.trim_start_matches("/ui/destroy/").to_string();
            return Ok(ui_destroy(&req, engine, &sessions, branch).await);
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
        _ => Ok(text(StatusCode::NOT_FOUND, "not found")),
    }
}
```

Add the UI handlers at the bottom of `src/api.rs`. Helpers first:

```rust
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

/// The session token from the request's Cookie header, if any.
fn session_of(req: &Request<Incoming>, sessions: &Sessions) -> bool {
    let raw = req.headers().get(COOKIE).and_then(|v| v.to_str().ok());
    cookie_value(raw, SESSION_COOKIE)
        .map(|t| sessions.validate(&t))
        .unwrap_or(false)
}

/// None → the dashboard is not configured; every UI route answers 503.
fn dashboard_enabled(settings: &Settings) -> bool {
    settings.dashboard_password.is_some()
}
```

Then the handlers:

```rust
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
    let Some(expected) = settings.dashboard_password.as_ref() else {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    };
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return html(StatusCode::BAD_REQUEST, dashboard::login_page(Some("Bad request"))),
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
    html(StatusCode::OK, dashboard::login_page(Some("Invalid password")))
}

fn ui_logout(req: &Request<Incoming>, sessions: &Sessions) -> Response<ApiBody> {
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
        .header(SET_COOKIE, format!("{SESSION_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=0"))
        .body(Full::new(Bytes::new()))
        .expect("logout redirect is always valid")
}

fn ui_root<R: ContainerRuntime>(
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
    html(StatusCode::OK, dashboard::dashboard_page(&engine.deployments()))
}

async fn ui_destroy<R: ContainerRuntime>(
    req: &Request<Incoming>,
    engine: Arc<Engine<R>>,
    sessions: &Sessions,
    branch: String,
) -> Response<ApiBody> {
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
```

Note the `ui_destroy` signature takes `&Request` but also needs the body-less request; since `POST /ui/destroy` carries no body we borrow the request for its cookie only. `handle_api` calls it as `ui_destroy(&req, engine, &sessions, branch)` — adjust the borrow so `req` isn't moved earlier (it isn't; the UI match borrows `&method`/`path` and `req` is still owned).

- [ ] **Step 4: Update `main.rs` to read the password**

In `src/main.rs`, add to the `Settings` construction:

```rust
dashboard_password: std::env::var("HOSTER_DASHBOARD_PASSWORD").ok(),
```

(Right after `token: ...`. It's optional — `None` when unset.)

- [ ] **Step 5: Run tests, expect pass**

Run: `cargo test --test dashboard && cargo test`
Expected: the 6 dashboard integration tests pass; the existing api tests and all others still pass.

- [ ] **Step 6: Full gate**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all green. `cargo fmt` if needed.

- [ ] **Step 7: Commit**

```bash
git add src/api.rs src/main.rs tests/dashboard.rs
git commit -m "feat: cookie-authenticated dashboard UI routes"
```

---

## Done when

- `cargo test` green including `tests/dashboard.rs`.
- `GET /` serves the dashboard (logged in) or redirects to `/login`; wrong password shows an error and sets no cookie; destroy works only with a valid session.
- The bearer API is unchanged and rejects the session cookie.
- With no `HOSTER_DASHBOARD_PASSWORD`, UI routes return 503 and the API is unaffected.

## Deploy (after merge)

Rebuild on the server, set `HOSTER_DASHBOARD_PASSWORD` in `/etc/hoster.env`,
`systemctl restart hoster`. `hoster.odinvestor.net` then shows the login page.

## Next milestone

TLS-in-proxy or TTL/reaping (which brings the Pin button and a real database),
then per-project tokens.
