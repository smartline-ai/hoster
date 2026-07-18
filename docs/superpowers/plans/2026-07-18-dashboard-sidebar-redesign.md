# Dashboard Sidebar Redesign + Live Logs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single-scroll dashboard with a master-detail sidebar-navigation console (Overview / per-project / Settings), refine its visual identity, and add live-streaming container logs.

**Architecture:** Server-rendered HTML with real URLs — no SPA, no build step. `src/dashboard.rs` is replaced by a focused `src/ui/` module (one file per view). The API's buffered response body is generalized to a boxed body so the new SSE log endpoint can stream. The only client-side JavaScript is a small script scoped to the project page that opens an `EventSource` per expanded log panel.

**Tech Stack:** Rust, hyper 1, http-body-util (`BoxBody`, `StreamBody`, `Full`), bollard 0.18 (Docker), futures-util, async-trait, tokio.

## Global Constraints

- No new heavy dependencies; use what `Cargo.toml` already has (hyper, http-body-util, bollard, futures-util, async-trait, bytes, tokio, serde).
- Secrets are never rendered: env values, the registry password, the API token, and the dashboard password must never appear in any HTML or log frame. Only masked bullets / usernames / hosts as today.
- Every dynamic value rendered into HTML passes through `html_escape`.
- All UI page routes and the SSE route are cookie-authenticated (`hoster_session`); unauthenticated page requests redirect to `/login`, unauthenticated SSE requests return `401`.
- Preserve the existing CSS design tokens (`--bg`, `--panel`, `--accent`, status colors, the `@media(prefers-color-scheme:light)` overrides, and `@media(prefers-reduced-motion:reduce)`); refine, do not discard.
- Run `cargo test` and `cargo clippy --all-targets` clean after every task. Format with `cargo fmt`.

---

## File Structure

**Created:**
- `src/ui/mod.rs` — module wiring + `page(title, body)` HTML shell (`<head>`/`<title>`) + public entry points.
- `src/ui/style.rs` — the CSS `STYLE` const (refined; adds sidebar-layout classes).
- `src/ui/components.rs` — shared primitives: `html_escape`, `plural`, brand `MARK`, `EXT_ICON`, status helpers.
- `src/ui/shell.rs` — `app_shell(active, projects, content)`: left rail + right pane, active-item highlight.
- `src/ui/overview.rs` — `overview_body(deployments)`.
- `src/ui/project.rs` — `project_body(project, deployments, env)` incl. env/registry/log panels + the log `<script>`.
- `src/ui/settings.rs` — `settings_body(settings)` (read-only, no secrets).
- `src/ui/login.rs` — `login_page(error)`.

**Modified:**
- `src/lib.rs` — replace `pub mod dashboard;` with `pub mod ui;`.
- `src/runtime.rs` — add `LogStream` alias + `logs(...)` to `ContainerRuntime`; implement on `FakeRuntime`.
- `src/docker.rs` — implement `logs(...)` via bollard.
- `src/engine.rs` — add `service_logs(project, branch, service, follow, tail)`.
- `src/api.rs` — generalize `ApiBody` to a boxed body; rewire UI routes (`/`, `/p/{project}`, `/settings`, redirects); add SSE `/p/{project}/logs/{branch}/{service}`.

**Deleted:**
- `src/dashboard.rs` (its logic moves into `src/ui/`, done in Task 8).

---

### Task 1: Generalize the API response body to a boxed body

Enables streaming responses (SSE) to share the `handle_api` return type with buffered responses. Pure refactor — no behavior change; the existing test suite is the guard.

**Files:**
- Modify: `src/api.rs` (type alias `ApiBody`, body constructors, two in-test builders)

**Interfaces:**
- Produces: `pub type ApiBody = http_body_util::combinators::BoxBody<bytes::Bytes, BoxError>;` and `pub type BoxError = Box<dyn std::error::Error + Send + Sync>;` and helper `fn full(bytes: Bytes) -> ApiBody`.

- [ ] **Step 1: Run the existing suite to confirm a green baseline**

Run: `cargo test`
Expected: PASS (all tests currently green).

- [ ] **Step 2: Change the body type alias and imports**

In `src/api.rs`, update the import line:

```rust
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
```

Replace the `ApiBody` alias:

```rust
/// Boxed response body. Buffered responses wrap `Full`; the SSE log endpoint
/// wraps a `StreamBody`. Boxing lets both share one `Response<ApiBody>` type.
pub type ApiBody = BoxBody<Bytes, BoxError>;

/// Error type carried by streaming bodies (e.g. a Docker log stream failing
/// mid-flight). Buffered `Full` bodies are infallible and never produce one.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Wrap buffered bytes as a boxed body. `Full` is infallible, so its `Never`
/// error is mapped away.
fn full(bytes: Bytes) -> ApiBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}
```

- [ ] **Step 3: Route every buffered constructor through `full`**

In `src/api.rs`, replace each `Full::new(<expr>)` used as a response body with `full(<expr>)`. There are constructors at (current lines) `text`, `text_owned`, `json_bytes`, `handle_teardown`'s empty body (~374), `html`, `redirect` (~399), `ui_login_submit`'s redirect body (~460), `ui_logout`'s redirect body (~492), and the two in-test response builders (~745, ~846). Each `.body(Full::new(x))` becomes `.body(full(x))`. Example:

```rust
fn text(status: StatusCode, body: &'static str) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(full(Bytes::from(body)))
        .expect("static response is always valid")
}
```

- [ ] **Step 4: Build and test**

Run: `cargo test`
Expected: PASS. If the compiler flags a remaining `Full::new(...)` in a `.body(...)`, convert it to `full(...)`.

- [ ] **Step 5: Clippy + commit**

Run: `cargo clippy --all-targets && cargo fmt`
Then:

```bash
git add src/api.rs
git commit -m "refactor(api): box the response body to allow streaming responses"
```

---

### Task 2: Add `logs` to the container runtime

**Files:**
- Modify: `src/runtime.rs` (trait + `FakeRuntime` impl + a test)
- Modify: `src/docker.rs` (Docker impl)

**Interfaces:**
- Produces: on `ContainerRuntime`:
  ```rust
  async fn logs(&self, container_id: &str, follow: bool, tail: usize) -> anyhow::Result<LogStream>;
  ```
  and `pub type LogStream = std::pin::Pin<Box<dyn futures_util::Stream<Item = anyhow::Result<String>> + Send>>;`
- Consumes: nothing from earlier tasks.

- [ ] **Step 1: Write the failing test (fake returns canned lines)**

In `src/runtime.rs` `mod tests`, add:

```rust
#[tokio::test]
async fn fake_runtime_streams_canned_log_lines() {
    use futures_util::StreamExt;
    let rt = FakeRuntime::new();
    let mut stream = rt.logs("fake-1", false, 100).await.unwrap();
    let mut lines = Vec::new();
    while let Some(item) = stream.next().await {
        lines.push(item.unwrap());
    }
    assert!(!lines.is_empty());
    assert!(lines.iter().any(|l| l.contains("listening")));
}
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test fake_runtime_streams_canned_log_lines`
Expected: FAIL — `logs` not found on `ContainerRuntime` / `FakeRuntime`.

- [ ] **Step 3: Add the type alias and trait method**

In `src/runtime.rs`, near the top add:

```rust
/// A live stream of already-decoded log lines (no trailing newline per item).
pub type LogStream =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = anyhow::Result<String>> + Send>>;
```

Add to the `ContainerRuntime` trait (after `list_by_label`):

```rust
    /// Stream a container's logs. `follow` keeps the stream open for new
    /// output; `tail` is how many existing lines to replay first.
    async fn logs(&self, container_id: &str, follow: bool, tail: usize) -> anyhow::Result<LogStream>;
```

- [ ] **Step 4: Implement on `FakeRuntime`**

In `src/runtime.rs`, inside `impl ContainerRuntime for FakeRuntime`, add:

```rust
    async fn logs(&self, container_id: &str, _follow: bool, _tail: usize) -> anyhow::Result<LogStream> {
        // A canned, finite stream so tests and no-Docker dev work without a
        // daemon. The container id is echoed so callers can tell streams apart.
        let lines = vec![
            format!("[{container_id}] server listening on :8080"),
            format!("[{container_id}] GET / 200"),
            format!("[{container_id}] GET /health 200"),
        ];
        let stream = futures_util::stream::iter(lines.into_iter().map(Ok));
        Ok(Box::pin(stream))
    }
```

- [ ] **Step 5: Run the fake test — expect PASS**

Run: `cargo test fake_runtime_streams_canned_log_lines`
Expected: PASS.

- [ ] **Step 6: Implement on `DockerRuntime`**

In `src/docker.rs`, add these imports to the existing `bollard::container` use group: `LogsOptions, LogOutput`. Then add to `impl ContainerRuntime for DockerRuntime`:

```rust
    async fn logs(&self, container_id: &str, follow: bool, tail: usize) -> anyhow::Result<LogStream> {
        use futures_util::StreamExt;
        let options = LogsOptions::<String> {
            follow,
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            timestamps: false,
            ..Default::default()
        };
        let raw = self.docker.logs(container_id, Some(options));
        // Map each Docker log chunk to a UTF-8 line, dropping the trailing
        // newline bollard includes. A stream error becomes a stream item error.
        let mapped = raw.map(|chunk| {
            chunk
                .map(|out: LogOutput| {
                    String::from_utf8_lossy(&out.into_bytes())
                        .trim_end_matches('\n')
                        .to_string()
                })
                .map_err(anyhow::Error::from)
        });
        Ok(Box::pin(mapped))
    }
```

Update the `use crate::runtime::...` line in `src/docker.rs` to include `LogStream`:

```rust
use crate::runtime::{ContainerRuntime, ContainerSpec, LogStream, RunningContainer};
```

- [ ] **Step 7: Build everything and test**

Run: `cargo test && cargo clippy --all-targets`
Expected: PASS. (The Docker impl compiles even where no daemon runs; only real Docker tests need a daemon, and none are added here.)

- [ ] **Step 8: Format + commit**

```bash
cargo fmt
git add src/runtime.rs src/docker.rs
git commit -m "feat(runtime): stream container logs via a new logs() method"
```

---

### Task 3: Resolve a service container and stream its logs from the engine

**Files:**
- Modify: `src/engine.rs` (add `service_logs`, add a test)

**Interfaces:**
- Consumes: `ContainerRuntime::logs`, `LogStream` (Task 2); `labels::{PROJECT, BRANCH, SERVICE}`; `settings::sanitize_branch`.
- Produces: on `Engine<R>`:
  ```rust
  pub async fn service_logs(&self, project: &str, branch: &str, service: &str, follow: bool, tail: usize) -> anyhow::Result<LogStream>;
  ```

- [ ] **Step 1: Write the failing test**

In `src/engine.rs` `mod tests`, add (mirroring existing tests that deploy `TWO_SERVICE` with a `FakeRuntime`):

```rust
#[tokio::test]
async fn service_logs_streams_for_a_deployed_service() {
    use futures_util::StreamExt;
    let rt = std::sync::Arc::new(FakeRuntime::new());
    let eng = test_engine(rt.clone());
    eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();

    let mut stream = eng.service_logs("p", "b1", "backend", false, 100).await.unwrap();
    let first = stream.next().await.expect("at least one line").unwrap();
    assert!(!first.is_empty());
}

#[tokio::test]
async fn service_logs_errors_for_an_unknown_service() {
    let rt = std::sync::Arc::new(FakeRuntime::new());
    let eng = test_engine(rt.clone());
    eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
    assert!(eng.service_logs("p", "b1", "nope", false, 100).await.is_err());
}
```

> If a `test_engine(rt)` helper does not already exist in the test module, use the same `Engine` construction the neighbouring tests use (e.g. `Engine::new(...)`) — match the existing pattern exactly rather than introducing a new constructor.

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test service_logs_`
Expected: FAIL — `service_logs` not found.

- [ ] **Step 3: Implement `service_logs`**

In `src/engine.rs`, add to `impl<R: ContainerRuntime> Engine<R>`:

```rust
    /// Stream one service's container logs. Resolves the container by its
    /// project/branch/service labels (branch sanitized the same way deploys
    /// are), erroring if no such running container exists.
    pub async fn service_logs(
        &self,
        project: &str,
        branch: &str,
        service: &str,
        follow: bool,
        tail: usize,
    ) -> anyhow::Result<LogStream> {
        let branch = sanitize_branch(branch);
        let containers = self.runtime.list_by_label(labels::PROJECT).await?;
        let target = containers
            .into_iter()
            .find(|c| {
                c.labels.get(labels::PROJECT).map(String::as_str) == Some(project)
                    && c.labels.get(labels::BRANCH).map(String::as_str) == Some(branch.as_str())
                    && c.labels.get(labels::SERVICE).map(String::as_str) == Some(service)
            })
            .ok_or_else(|| anyhow::anyhow!("no running container for {project}/{branch}/{service}"))?;
        self.runtime.logs(&target.id, follow, tail).await
    }
```

Ensure `LogStream` is imported in `src/engine.rs`:

```rust
use crate::runtime::{ContainerRuntime, LogStream};
```

(Merge with the existing `use crate::runtime::...` line rather than duplicating it.)

- [ ] **Step 4: Run the tests — expect PASS**

Run: `cargo test service_logs_`
Expected: PASS (both).

- [ ] **Step 5: Clippy, format, commit**

```bash
cargo clippy --all-targets && cargo fmt
git add src/engine.rs
git commit -m "feat(engine): resolve a service container and stream its logs"
```

---

### Task 4: UI module scaffold — style, components, shell, login

Builds the new `ui` module **alongside** the old `dashboard` module (both compile). No routes change yet, so `dashboard.rs` still serves `/`. Establishes the sidebar frame and refined styling.

**Files:**
- Create: `src/ui/mod.rs`, `src/ui/style.rs`, `src/ui/components.rs`, `src/ui/shell.rs`, `src/ui/login.rs`
- Modify: `src/lib.rs` (add `pub mod ui;` — keep `pub mod dashboard;` for now)

**Interfaces:**
- Produces:
  - `ui::components::html_escape(&str) -> String`, `plural(usize, &str, &str) -> String`, consts `MARK`, `EXT_ICON`.
  - `ui::shell::Nav` enum: `Overview`, `Project(&str)`, `Settings`.
  - `ui::shell::app_shell(active: Nav, projects: &[&str], content: &str) -> String` — returns a full page (calls `page`).
  - `ui::login_page(error: Option<&str>) -> String`.
  - `ui::page(title: &str, body: &str) -> String`.

- [ ] **Step 1: Register the module**

In `src/lib.rs`, add below `pub mod template;` (keep `pub mod dashboard;`):

```rust
pub mod ui;
```

- [ ] **Step 2: Create `src/ui/components.rs`**

```rust
//! Shared HTML primitives used across every UI view.

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

/// `1 branch` / `2 branches` — plain-English counts for meta lines.
pub fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The brand mark: a single host fanning out to three branch endpoints.
pub const MARK: &str = r##"<svg class="mark" viewBox="0 0 32 32" fill="none" aria-hidden="true"><defs><linearGradient id="hg" x1="0" y1="0" x2="32" y2="32"><stop stop-color="#7b8cff"/><stop offset="1" stop-color="#a97bff"/></linearGradient></defs><circle cx="6" cy="16" r="3.2" fill="url(#hg)"/><circle cx="26" cy="7" r="2.6" fill="currentColor" opacity=".85"/><circle cx="26" cy="16" r="2.6" fill="currentColor" opacity=".85"/><circle cx="26" cy="25" r="2.6" fill="currentColor" opacity=".85"/><path d="M9 16H16M16 16V7H23M16 16H23M16 16V25H23" stroke="url(#hg)" stroke-width="1.6" stroke-linecap="round"/></svg>"##;

/// External-link glyph for URL chips.
pub const EXT_ICON: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M7 17 17 7M9 7h8v8"/></svg>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(html_escape("<script>&\"'"), "&lt;script&gt;&amp;&quot;&#39;");
    }
}
```

- [ ] **Step 3: Create `src/ui/style.rs`**

Port the existing `STYLE` const from `src/dashboard.rs` (lines 24–152) verbatim as a starting point, then append the sidebar-layout classes below. Keep all existing tokens and the light-mode + reduced-motion media queries.

```rust
//! The dashboard's CSS. One const, inlined into every page's <head>.

pub const STYLE: &str = r#"
<PASTE the existing :root{...} ... reduced-motion block from dashboard.rs here, UNCHANGED>

/* --- app shell: sidebar + content (master-detail) --- */
.app{display:grid;grid-template-columns:248px 1fr;min-height:100dvh}
.rail{position:sticky;top:0;align-self:start;height:100dvh;display:flex;flex-direction:column;
  gap:.35rem;padding:1.1rem .8rem;border-right:1px solid var(--line);
  background:color-mix(in srgb,var(--panel) 55%,var(--bg))}
.rail .brand{display:flex;align-items:center;gap:.55rem;padding:.35rem .55rem 1rem}
.rail .wordmark{font-weight:680;letter-spacing:-.01em;font-size:1.02rem}
.nav-label{font-size:.66rem;letter-spacing:.16em;text-transform:uppercase;color:var(--faint);
  font-weight:600;padding:.9rem .6rem .35rem}
.nav-item{display:flex;align-items:center;gap:.55rem;padding:.5rem .6rem;border-radius:9px;
  color:var(--muted);font-size:.86rem;font-weight:540;transition:.13s}
.nav-item:hover{background:var(--panel-2);color:var(--ink);text-decoration:none}
.nav-item.active{background:color-mix(in srgb,var(--accent) 16%,transparent);color:var(--ink)}
.nav-item.active .glyph{color:var(--accent)}
.nav-item .glyph{color:var(--faint);font-size:.9rem;width:1rem;text-align:center}
.nav-spacer{flex:1}
.rail form{margin:0}
.content{min-width:0;padding:clamp(1rem,3vw,2.2rem) clamp(1rem,4vw,2.6rem) 4rem;max-width:1100px}
.page-head{display:flex;align-items:baseline;gap:.8rem;flex-wrap:wrap;margin-bottom:.4rem}
.page-head h1{font-size:1.4rem;letter-spacing:-.02em;font-weight:660}
.page-sub{color:var(--muted);font-size:.85rem}
.stat-row{display:flex;gap:1.6rem;margin:1.2rem 0 .4rem}
.stat{display:flex;flex-direction:column;gap:.15rem}
.stat .n{font-size:1.5rem;font-weight:680;letter-spacing:-.02em}
.stat .l{font-size:.72rem;letter-spacing:.1em;text-transform:uppercase;color:var(--muted)}
/* --- live log panel --- */
details.logs{margin-top:.55rem}
details.logs>summary{list-style:none;cursor:pointer;display:inline-flex;align-items:center;gap:.35rem;
  font-size:.75rem;color:var(--muted);font-weight:560;user-select:none}
details.logs>summary::-webkit-details-marker{display:none}
.logterm{margin-top:.5rem;background:#0a0c11;border:1px solid var(--line-2);border-radius:9px;
  padding:.7rem .8rem;max-height:260px;overflow:auto;font-family:var(--mono);font-size:.74rem;
  line-height:1.5;color:#c8d0dc;white-space:pre-wrap;word-break:break-word}
.logterm .ph{color:var(--faint)}
@media(max-width:820px){.app{grid-template-columns:1fr}
  .rail{position:static;height:auto;flex-direction:row;flex-wrap:wrap;align-items:center;height:auto}
  .nav-spacer{display:none}}
"#;
```

- [ ] **Step 4: Create `src/ui/mod.rs` with `page` + entry points**

```rust
//! The operator dashboard UI: a server-rendered, sidebar-navigation console.
//! Each view lives in its own submodule; this module owns the HTML shell and
//! the public entry points the API layer calls.

mod components;
mod login;
mod overview;
mod project;
mod settings;
mod shell;
mod style;

pub use components::html_escape;
pub use login::login_page;

use crate::engine::DeploymentView;
use crate::secrets::MaskedProject;
use crate::settings::Settings;
use shell::{Nav, app_shell};

/// Wrap a rendered body in the full HTML document with inlined styles.
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{}</style></head><body>{body}</body></html>",
        html_escape(title),
        style::STYLE,
    )
}

/// Sorted, de-duplicated project names across deployments and env — the
/// sidebar's project list. Shared by every page so the rail is identical.
fn project_names<'a>(deployments: &'a [DeploymentView], env: &'a [MaskedProject]) -> Vec<&'a str> {
    let mut set = std::collections::BTreeSet::new();
    for d in deployments {
        set.insert(d.project.as_str());
    }
    for p in env {
        set.insert(p.project.as_str());
    }
    set.into_iter().collect()
}

/// `GET /` — the Overview page.
pub fn overview_page(deployments: &[DeploymentView], env: &[MaskedProject]) -> String {
    let projects = project_names(deployments, env);
    let body = overview::overview_body(deployments);
    app_shell(Nav::Overview, &projects, &body)
}

/// `GET /p/{project}` — one project's deployments, env, and registry.
pub fn project_page(project: &str, deployments: &[DeploymentView], env: &[MaskedProject]) -> String {
    let projects = project_names(deployments, env);
    let deps: Vec<&DeploymentView> = deployments.iter().filter(|d| d.project == project).collect();
    let body = project::project_body(project, &deps, env);
    app_shell(Nav::Project(project), &projects, &body)
}

/// `GET /settings` — read-only system information.
pub fn settings_page(
    settings: &Settings,
    deployments: &[DeploymentView],
    env: &[MaskedProject],
) -> String {
    let projects = project_names(deployments, env);
    let body = settings::settings_body(settings);
    app_shell(Nav::Settings, &projects, &body)
}
```

- [ ] **Step 5: Create `src/ui/shell.rs`**

```rust
//! The persistent app frame: a left navigation rail + right content pane.

use std::fmt::Write;

use crate::ui::components::{MARK, html_escape};
use crate::ui::page;

/// Which nav item is active on the current page.
pub enum Nav<'a> {
    Overview,
    Project(&'a str),
    Settings,
}

/// Render the full page: sidebar rail (Overview, projects, Settings, Sign out)
/// wrapping the given content on the right. The active item is highlighted.
pub fn app_shell(active: Nav, projects: &[&str], content: &str) -> String {
    let mut rail = format!(
        "<nav class=\"rail\"><a class=\"brand\" href=\"/\">{MARK}<span class=\"wordmark\">hoster</span></a>",
    );

    let overview_cls = if matches!(active, Nav::Overview) { "nav-item active" } else { "nav-item" };
    let _ = write!(
        rail,
        "<a class=\"{overview_cls}\" href=\"/\"><span class=\"glyph\">\u{25d0}</span>Overview</a>",
    );

    rail.push_str("<div class=\"nav-label\">Projects</div>");
    if projects.is_empty() {
        rail.push_str("<span class=\"nav-item\" style=\"color:var(--faint)\">None yet</span>");
    }
    for p in projects {
        let active_here = matches!(active, Nav::Project(cur) if cur == *p);
        let cls = if active_here { "nav-item active" } else { "nav-item" };
        let esc = html_escape(p);
        let _ = write!(
            rail,
            "<a class=\"{cls}\" href=\"/p/{esc}\"><span class=\"glyph\">\u{25c8}</span>{esc}</a>",
        );
    }

    let settings_cls = if matches!(active, Nav::Settings) { "nav-item active" } else { "nav-item" };
    let _ = write!(
        rail,
        "<div class=\"nav-spacer\"></div>\
<a class=\"{settings_cls}\" href=\"/settings\"><span class=\"glyph\">\u{2699}</span>Settings</a>\
<form method=\"post\" action=\"/logout\"><button class=\"nav-item\" type=\"submit\" \
style=\"width:100%;background:none;border:0;text-align:left\">\
<span class=\"glyph\">\u{21aa}</span>Sign out</button></form></nav>",
    );

    let body = format!("<div class=\"app\">{rail}<main class=\"content\">{content}</main></div>");
    page("hoster", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_lists_projects_and_marks_the_active_one() {
        let html = app_shell(Nav::Project("blog"), &["blog", "api"], "BODY");
        assert!(html.contains("href=\"/p/blog\""));
        assert!(html.contains("href=\"/p/api\""));
        // the active project's link carries the active class
        assert!(html.contains("nav-item active\" href=\"/p/blog\""));
        assert!(html.contains("BODY"));
        assert!(html.contains("action=\"/logout\""));
    }

    #[test]
    fn overview_is_active_on_the_overview_shell() {
        let html = app_shell(Nav::Overview, &["blog"], "X");
        assert!(html.contains("nav-item active\" href=\"/\""));
    }
}
```

> Note: `components` and `page` must be reachable from `shell`. They are, via `crate::ui::…`. Mark `mod components;` etc. as `pub(crate)` in `mod.rs` if the compiler complains about visibility — change `mod components;` to `pub(crate) mod components;` and likewise for `style`.

- [ ] **Step 6: Create `src/ui/login.rs`**

```rust
//! The sign-in page — the one view rendered outside the app shell.

use crate::ui::components::MARK;
use crate::ui::{html_escape, page};

/// The login form. `error` renders a message above the form when a prior
/// attempt failed.
pub fn login_page(error: Option<&str>) -> String {
    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let body = format!(
        "<div class=\"login-wrap\"><div class=\"login-card\">{MARK}\
<h1>hoster</h1><p>Sign in to the deploy console.</p>{err}\
<form class=\"login-form\" method=\"post\" action=\"/login\">\
<input type=\"password\" name=\"password\" placeholder=\"Password\" autocomplete=\"current-password\" autofocus>\
<button class=\"btn primary\" type=\"submit\">Sign in</button></form></div></div>"
    );
    page("hoster — sign in", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_has_password_form() {
        let html = login_page(None);
        assert!(html.contains("action=\"/login\""));
        assert!(html.contains("type=\"password\""));
    }

    #[test]
    fn login_page_shows_error() {
        assert!(login_page(Some("Invalid password")).contains("Invalid password"));
    }
}
```

- [ ] **Step 7: Add temporary empty submodules so the module compiles**

Create `src/ui/overview.rs`, `src/ui/project.rs`, `src/ui/settings.rs` as stubs that the next tasks fill in. Each must expose the function `mod.rs` calls:

`src/ui/overview.rs`:

```rust
//! The Overview page body — filled in Task 5.
use crate::engine::DeploymentView;

pub fn overview_body(_deployments: &[DeploymentView]) -> String {
    String::new()
}
```

`src/ui/project.rs`:

```rust
//! The Project page body — filled in Task 6.
use crate::engine::DeploymentView;
use crate::secrets::MaskedProject;

pub fn project_body(_project: &str, _deps: &[&DeploymentView], _env: &[MaskedProject]) -> String {
    String::new()
}
```

`src/ui/settings.rs`:

```rust
//! The Settings page body — filled in Task 7.
use crate::settings::Settings;

pub fn settings_body(_settings: &Settings) -> String {
    String::new()
}
```

- [ ] **Step 8: Build + test**

Run: `cargo test`
Expected: PASS. New `ui` tests (shell, login, escape) run; old `dashboard` tests still run.

- [ ] **Step 9: Clippy, format, commit**

```bash
cargo clippy --all-targets && cargo fmt
git add src/lib.rs src/ui/
git commit -m "feat(ui): sidebar-nav shell, styles, components, and login view"
```

---

### Task 5: Overview page body

**Files:**
- Modify: `src/ui/overview.rs`

**Interfaces:**
- Consumes: `DeploymentView { project, branch, status, urls, config }`, `ui::components::{html_escape, plural, EXT_ICON}`, `ui::project::status_word` (defined here as a shared helper — see note).
- Produces: `overview::overview_body(deployments: &[DeploymentView]) -> String`.

- [ ] **Step 1: Write the failing test**

Replace the stub `src/ui/overview.rs` test section by adding at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn view(project: &str, branch: &str, status: &str) -> DeploymentView {
        DeploymentView {
            project: project.to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            urls: BTreeMap::new(),
            config: None,
        }
    }

    #[test]
    fn overview_counts_and_lists_across_projects() {
        let body = overview_body(&[
            view("blog", "main", "running"),
            view("api", "feat-x", "failed: boom"),
        ]);
        assert!(body.contains("blog"));
        assert!(body.contains("api"));
        assert!(body.contains("href=\"/p/blog\""));
        assert!(body.contains("href=\"/p/api\""));
        // aggregate running count is present
        assert!(body.contains("Running"));
        // a failed deploy still lists, its reason escaped/omitted from status word
        assert!(body.contains("failed"));
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test overview_counts_and_lists_across_projects`
Expected: FAIL — body is empty, assertions fail.

- [ ] **Step 3: Implement `overview_body`**

Replace the stub body of `src/ui/overview.rs` (keep the module doc-comment and the test module) with:

```rust
use std::fmt::Write;

use crate::engine::DeploymentView;
use crate::ui::components::{EXT_ICON, html_escape, plural};

/// Reduce a status string to its leading word (`"failed: boom"` -> `"failed"`).
pub(crate) fn status_word(status: &str) -> &str {
    match status.split_once(':') {
        Some((w, _)) => w.trim(),
        None => status.trim(),
    }
}

pub fn overview_body(deployments: &[DeploymentView]) -> String {
    let projects: std::collections::BTreeSet<&str> =
        deployments.iter().map(|d| d.project.as_str()).collect();
    let running = deployments
        .iter()
        .filter(|d| status_word(&d.status) == "running")
        .count();

    let mut body = format!(
        "<div class=\"page-head\"><h1>Overview</h1></div>\
<div class=\"stat-row\">\
<div class=\"stat\"><span class=\"n\">{}</span><span class=\"l\">Projects</span></div>\
<div class=\"stat\"><span class=\"n\">{}</span><span class=\"l\">Deployments</span></div>\
<div class=\"stat\"><span class=\"n\">{running}</span><span class=\"l\">Running</span></div>\
</div>",
        projects.len(),
        deployments.len(),
    );

    if deployments.is_empty() {
        body.push_str(
            "<div class=\"empty\" style=\"margin-top:1.4rem\">No deployments yet. \
Deploy a branch or add environment variables to a project to get started.</div>",
        );
        return body;
    }

    body.push_str("<div class=\"col-label\" style=\"margin-top:1.6rem\">All deployments</div>");
    for d in deployments {
        let word = status_word(&d.status);
        let project = html_escape(&d.project);
        let branch = html_escape(&d.branch);
        let _ = write!(
            body,
            "<a class=\"deploy is-{word}\" href=\"/p/{project}\" style=\"text-decoration:none\">\
<span class=\"led\"></span><div class=\"deploy-main\"><div class=\"deploy-row1\">\
<span class=\"branch\">{branch}</span><span class=\"pill {word}\"><span class=\"dot\"></span>{word}</span>\
<span class=\"panel-meta\">{project}</span></div>",
        );
        if word != "failed" && !d.urls.is_empty() {
            body.push_str("<div class=\"urls\">");
            for u in d.urls.values() {
                let e = html_escape(u);
                let _ = write!(
                    body,
                    "<span class=\"chip\"><span class=\"host\">{e}</span>{EXT_ICON}</span>",
                );
            }
            body.push_str("</div>");
        }
        body.push_str("</div></a>");
    }

    let _ = plural(0, "", ""); // keep import used if URLs branch is cold; harmless
    body
}
```

> If `cargo clippy` warns that `plural` is unused, delete the `use ... plural` import and the throwaway line instead of keeping the no-op. Prefer removing the unused import.

- [ ] **Step 4: Run the test — expect PASS**

Run: `cargo test overview_counts_and_lists_across_projects`
Expected: PASS.

- [ ] **Step 5: Clippy, format, commit**

```bash
cargo clippy --all-targets && cargo fmt
git add src/ui/overview.rs
git commit -m "feat(ui): overview page with cross-project deployment list"
```

---

### Task 6: Project page body with env, registry, and live-log panels

Ports the existing `render_deployments` / `render_config` / `render_environment` / `render_registry` from `dashboard.rs` into `project.rs`, updates form redirects' sibling markup, and adds a per-service log `<details>` panel plus the scoped `EventSource` script.

**Files:**
- Modify: `src/ui/project.rs`

**Interfaces:**
- Consumes: `DeploymentView`, `MaskedProject` (`.project`, `.vars: [MaskedVar{key, services}]`, `.registry: Option<MaskedRegistry{registry, username}>`), `overview::status_word`, components.
- Produces: `project::project_body(project: &str, deps: &[&DeploymentView], env: &[MaskedProject]) -> String`.

- [ ] **Step 1: Write the failing tests**

Append to `src/ui/project.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::secrets::{MaskedRegistry, MaskedVar};
    use std::collections::BTreeMap;

    const CFG: &str = r#"{"project":"odin","services":{
        "backend":{"image":"reg/backend:abc","env":{"PORT":"8080"},"expose":{"port":8080}}
    }}"#;

    fn view(branch: &str, status: &str) -> DeploymentView {
        DeploymentView {
            project: "odin".to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            urls: BTreeMap::new(),
            config: config::parse(CFG).ok(),
        }
    }

    fn masked(vars: &[(&str, &[&str])], registry: Option<(&str, &str)>) -> MaskedProject {
        MaskedProject {
            project: "odin".to_string(),
            vars: vars
                .iter()
                .map(|(k, s)| MaskedVar { key: k.to_string(), services: s.iter().map(|x| x.to_string()).collect() })
                .collect(),
            registry: registry.map(|(r, u)| MaskedRegistry { registry: r.to_string(), username: u.to_string() }),
        }
    }

    #[test]
    fn renders_deployments_env_registry_and_log_toggle() {
        let deps = [view("b1", "running")];
        let refs: Vec<&DeploymentView> = deps.iter().collect();
        let env = [masked(&[("SECRET", &["backend"])], Some(("ghcr.io", "bot")))];
        let html = project_body("odin", &refs, &env);
        assert!(html.contains("b1"));
        assert!(html.contains("reg/backend:abc"));
        assert!(html.contains("SECRET"));
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("bot"));
        // masked value bullets, never a stored value
        assert!(html.contains('\u{2022}'));
        // per-service live-log stream URL
        assert!(html.contains("/p/odin/logs/b1/backend"));
        // forms redirect scope: destroy + var management under this project
        assert!(html.contains("action=\"/ui/destroy/b1\""));
        assert!(html.contains("action=\"/ui/projects/odin/vars\""));
        assert!(html.contains("action=\"/ui/projects/odin/registry\""));
    }

    #[test]
    fn escapes_failed_status_reason() {
        let deps = [view("b", "failed: <script>alert(1)</script>")];
        let refs: Vec<&DeploymentView> = deps.iter().collect();
        let html = project_body("odin", &refs, &[]);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test -p hoster renders_deployments_env_registry_and_log_toggle`
(Use the crate name from `Cargo.toml` if not `hoster`.)
Expected: FAIL — empty body.

- [ ] **Step 3: Implement `project_body` and its helpers**

Replace the stub in `src/ui/project.rs` (keep the doc comment + test module) with:

```rust
use std::fmt::Write;

use crate::engine::DeploymentView;
use crate::secrets::MaskedProject;
use crate::ui::components::{EXT_ICON, html_escape, plural};
use crate::ui::overview::status_word;

pub fn project_body(project: &str, deps: &[&DeploymentView], env: &[MaskedProject]) -> String {
    let vars = env.iter().find(|p| p.project == project).map(|p| p.vars.len()).unwrap_or(0);
    let running = deps.iter().filter(|d| status_word(&d.status) == "running").count();
    let esc = html_escape(project);

    let mut body = format!(
        "<div class=\"page-head\"><h1>{esc}</h1><span class=\"page-sub\">{} · {} · {}</span></div>",
        plural(deps.len(), "branch", "branches"),
        plural(running, "running", "running"),
        plural(vars, "variable", "variables"),
    );

    body.push_str("<section class=\"panel\"><div class=\"panel-body\" style=\"grid-template-columns:1fr\">");
    render_deployments(&mut body, project, deps);
    render_environment(&mut body, project, env);
    render_registry(&mut body, project, env);
    body.push_str("</div></section>");

    body.push_str(LOG_SCRIPT);
    body
}

fn render_deployments(body: &mut String, project: &str, deps: &[&DeploymentView]) {
    let _ = write!(
        body,
        "<div class=\"col\"><div class=\"col-label\">Deployments <span class=\"count\">{}</span></div>",
        deps.len()
    );
    if deps.is_empty() {
        body.push_str("<div class=\"empty\">No deployments yet.</div></div>");
        return;
    }
    let proj = html_escape(project);
    for d in deps {
        let branch = html_escape(&d.branch);
        let (word, reason) = match d.status.split_once(':') {
            Some((w, r)) => (w.trim(), Some(r.trim()).filter(|r| !r.is_empty())),
            None => (d.status.trim(), None),
        };
        let _ = write!(
            body,
            "<article class=\"deploy is-{word}\"><span class=\"led\"></span><div class=\"deploy-main\">\
<div class=\"deploy-row1\"><span class=\"branch\">{branch}</span>\
<span class=\"pill {word}\"><span class=\"dot\"></span>{word}</span></div>",
        );
        if word == "failed" {
            if let Some(r) = reason {
                let _ = write!(body, "<div class=\"reason\">{}</div>", html_escape(r));
            }
        } else if !d.urls.is_empty() {
            body.push_str("<div class=\"urls\">");
            for u in d.urls.values() {
                let e = html_escape(u);
                let _ = write!(body, "<a class=\"chip\" href=\"{e}\"><span class=\"host\">{e}</span>{EXT_ICON}</a>");
            }
            body.push_str("</div>");
        }
        render_config_and_logs(body, &proj, &branch, d);
        let _ = write!(
            body,
            "</div><form method=\"post\" action=\"/ui/destroy/{branch}\" \
onsubmit=\"return confirm('Destroy this branch?')\">\
<button class=\"btn danger\" type=\"submit\" title=\"Destroy branch\">Destroy</button></form></article>",
        );
    }
    body.push_str("</div>");
}

/// The per-service config block, each service carrying a live-log toggle. The
/// branch here is already HTML-escaped by the caller.
fn render_config_and_logs(body: &mut String, project: &str, branch: &str, d: &DeploymentView) {
    let Some(cfg) = &d.config else {
        body.push_str("<p class=\"reason\" style=\"color:var(--faint)\">configuration unavailable</p>");
        return;
    };
    body.push_str("<div class=\"svc-grid\">");
    for (name, svc) in &cfg.services {
        let svc_name = html_escape(name);
        let _ = write!(
            body,
            "<div class=\"svc\"><div class=\"svc-head\"><span class=\"svc-name\">{svc_name}</span>",
        );
        if let Some(exp) = &svc.expose {
            let _ = write!(body, "<span class=\"port\">:{}</span>", exp.port);
        }
        let _ = write!(body, "</div><code class=\"img\">{}</code>", html_escape(&svc.image));
        if !svc.env.is_empty() {
            body.push_str("<ul class=\"env-inline\">");
            for (k, v) in &svc.env {
                let _ = write!(
                    body,
                    "<li><span class=\"k\">{}</span><span class=\"eq\">=</span>{}</li>",
                    html_escape(k),
                    html_escape(v),
                );
            }
            body.push_str("</ul>");
        }
        // Live log panel: data-url is read by LOG_SCRIPT to open an EventSource
        // on expand and close it on collapse. project & branch are pre-escaped;
        // the service name path segment uses the raw name url-safe enough for
        // our service naming (alnum/dash) — escape it for the attribute too.
        let _ = write!(
            body,
            "<details class=\"logs\" data-url=\"/p/{project}/logs/{branch}/{svc_name}\">\
<summary><span class=\"chev\">\u{203a}</span> live logs</summary>\
<div class=\"logterm\"><span class=\"ph\">Connecting…</span></div></details>",
        );
        body.push_str("</div>");
    }
    body.push_str("</div>");
}

fn render_environment(body: &mut String, project: &str, env: &[MaskedProject]) {
    let vars = env.iter().find(|p| p.project == project).map(|p| &p.vars[..]).unwrap_or(&[]);
    let proj = html_escape(project);
    let _ = write!(
        body,
        "<aside class=\"col environment\"><div class=\"col-label\">Environment <span class=\"count\">{}</span></div>",
        vars.len()
    );
    if vars.is_empty() {
        body.push_str(
            "<div class=\"empty\">No variables yet. Add one below and it's injected into every deploy of this project.</div>",
        );
    } else {
        body.push_str("<div class=\"env-list\">");
        for v in vars {
            let key = html_escape(&v.key);
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{key}</span>\
<form method=\"post\" action=\"/ui/projects/{proj}/vars/{key}/delete\" \
onsubmit=\"return confirm('Delete this variable?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Delete variable\">\u{2715}</button></form>\
<div class=\"env-meta\"><span class=\"val\">\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}</span>",
            );
            if v.services.is_empty() {
                body.push_str("<span class=\"tag all\">all services</span>");
            } else {
                for s in &v.services {
                    let _ = write!(body, "<span class=\"tag\">{}</span>", html_escape(s));
                }
            }
            body.push_str("</div></div>");
        }
        body.push_str("</div>");
    }
    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/projects/{proj}/vars\">\
<input name=\"key\" placeholder=\"NEW_KEY\" required>\
<input name=\"value\" type=\"password\" placeholder=\"value\" autocomplete=\"off\" required>\
<input name=\"services\" placeholder=\"services \u{2014} comma-separated, blank = all\">\
<button class=\"btn primary\" type=\"submit\">Add variable</button></form></aside>",
    );
}

fn render_registry(body: &mut String, project: &str, env: &[MaskedProject]) {
    let cred = env.iter().find(|p| p.project == project).and_then(|p| p.registry.as_ref());
    let proj = html_escape(project);
    body.push_str("<aside class=\"col registry\"><div class=\"col-label\">Registry credential</div>");
    match cred {
        None => body.push_str("<div class=\"empty\">No registry credential. Public images only.</div>"),
        Some(c) => {
            let _ = write!(
                body,
                "<div class=\"env-list\"><div class=\"env-row\"><span class=\"k\">{registry}</span>\
<form method=\"post\" action=\"/ui/projects/{proj}/registry/delete\" \
onsubmit=\"return confirm('Remove this registry credential?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Remove registry credential\">\u{2715}</button></form>\
<div class=\"env-meta\"><span class=\"val\">\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}</span>\
<span class=\"tag\">{username}</span></div></div></div>",
                registry = html_escape(&c.registry),
                username = html_escape(&c.username),
            );
        }
    }
    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/projects/{proj}/registry\">\
<input name=\"registry\" placeholder=\"ghcr.io\" required>\
<input name=\"username\" placeholder=\"username\" required>\
<input name=\"password\" type=\"password\" placeholder=\"token or password\" required>\
<button class=\"btn primary\" type=\"submit\">Save credential</button></form></aside>",
    );
}

/// Scoped client script: on expanding a `.logs` panel, open an EventSource to
/// its data-url and append lines; on collapse, close it. The only JS in the app.
const LOG_SCRIPT: &str = r#"<script>
document.querySelectorAll('details.logs').forEach(function(d){
  var term=d.querySelector('.logterm'), es=null;
  d.addEventListener('toggle',function(){
    if(d.open){
      term.textContent='';
      es=new EventSource(d.dataset.url);
      es.onmessage=function(e){
        var atBottom=term.scrollHeight-term.scrollTop-term.clientHeight<20;
        term.textContent+=e.data+'\n';
        if(atBottom)term.scrollTop=term.scrollHeight;
      };
      es.onerror=function(){ if(es){es.close();es=null;} };
    } else if(es){ es.close(); es=null; }
  });
});
</script>"#;
```

- [ ] **Step 4: Run the tests — expect PASS**

Run: `cargo test renders_deployments_env_registry_and_log_toggle escapes_failed_status_reason`
Expected: PASS (both).

- [ ] **Step 5: Clippy, format, commit**

```bash
cargo clippy --all-targets && cargo fmt
git add src/ui/project.rs
git commit -m "feat(ui): project page with env, registry, and live-log panels"
```

---

### Task 7: Settings page body (read-only system info)

**Files:**
- Modify: `src/ui/settings.rs`

**Interfaces:**
- Consumes: `Settings { listen, api_listen, hostname_template, registry, token, dashboard_password }`, components.
- Produces: `settings::settings_body(settings: &Settings) -> String`.

- [ ] **Step 1: Write the failing test**

Append to `src/ui/settings.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            listen: "0.0.0.0:80".into(),
            api_listen: "0.0.0.0:8081".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "ghcr.io".into(),
            token: "super-secret-token".into(),
            dashboard_password: Some("hunter2".into()),
        }
    }

    #[test]
    fn shows_system_info_but_never_secrets() {
        let html = settings_body(&settings());
        assert!(html.contains("{service}-{branch}.dev.example.com"));
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("0.0.0.0:8081"));
        // secrets must never render
        assert!(!html.contains("super-secret-token"));
        assert!(!html.contains("hunter2"));
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test shows_system_info_but_never_secrets`
Expected: FAIL — empty body.

- [ ] **Step 3: Implement `settings_body`**

Replace the stub in `src/ui/settings.rs` (keep doc comment + tests):

```rust
use std::fmt::Write;

use crate::settings::Settings;
use crate::ui::components::html_escape;

pub fn settings_body(settings: &Settings) -> String {
    let mut body = String::from(
        "<div class=\"page-head\"><h1>Settings</h1>\
<span class=\"page-sub\">How this server is configured. Read-only — set at startup.</span></div>\
<section class=\"panel\"><div class=\"col\"><div class=\"env-list\">",
    );
    let row = |body: &mut String, label: &str, value: &str| {
        let _ = write!(
            body,
            "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
            html_escape(label),
            html_escape(value),
        );
    };
    row(&mut body, "Hostname template", &settings.hostname_template);
    row(&mut body, "Registry", &settings.registry);
    row(&mut body, "Proxy listen", &settings.listen);
    row(&mut body, "API listen", &settings.api_listen);
    row(&mut body, "Version", env!("CARGO_PKG_VERSION"));
    body.push_str("</div></div></section>");
    body
}
```

- [ ] **Step 4: Run the test — expect PASS**

Run: `cargo test shows_system_info_but_never_secrets`
Expected: PASS.

- [ ] **Step 5: Clippy, format, commit**

```bash
cargo clippy --all-targets && cargo fmt
git add src/ui/settings.rs
git commit -m "feat(ui): read-only settings page with system info"
```

---

### Task 8: Wire the new routes and remove `dashboard.rs`

Switches the API from the old single-page dashboard to the new per-view routes, retargets POST redirects to `/p/{project}`, and deletes `dashboard.rs`.

**Files:**
- Modify: `src/api.rs` (imports, route table, `ui_root` → `ui_overview`, add `ui_project`/`ui_settings`, redirects)
- Modify: `src/lib.rs` (remove `pub mod dashboard;`)
- Delete: `src/dashboard.rs`

**Interfaces:**
- Consumes: `ui::{login_page, overview_page, project_page, settings_page, html_escape}`.

- [ ] **Step 1: Repoint the module imports**

In `src/api.rs`, replace `use crate::dashboard;` with `use crate::ui;`. Anywhere the code calls `dashboard::login_page(...)`, change to `ui::login_page(...)`. Change `dashboard::dashboard_page(&deployments, &env)` (in `ui_root`) to `ui::overview_page(&deployments, &env)`.

- [ ] **Step 2: Rename `ui_root` → `ui_overview` and add project/settings handlers**

In `src/api.rs`, rename `ui_root` to `ui_overview` (keep its body, now returning `ui::overview_page`). Add two handlers next to it:

```rust
async fn ui_project<R: ContainerRuntime>(
    req: &Request<Incoming>,
    engine: &Engine<R>,
    settings: &Settings,
    sessions: &Sessions,
    project: &str,
) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    if !session_of(req, sessions) {
        return redirect("/login");
    }
    let deployments = engine.deployment_views().await.unwrap_or_default();
    let env = engine.store().list_masked();
    html(StatusCode::OK, ui::project_page(project, &deployments, &env))
}

async fn ui_settings<R: ContainerRuntime>(
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
    html(StatusCode::OK, ui::settings_page(settings, &deployments, &env))
}
```

- [ ] **Step 3: Extend the UI route table**

In `handle_api`, inside the UI `match (&method, path.as_str())` block, change the `/` arm and add `/settings` + `/p/{project}` (place the `/p/` prefix arm alongside the other prefix arms):

```rust
        (&Method::GET, "/") => return Ok(ui_overview(&req, &engine, &settings, &sessions).await),
        (&Method::GET, "/settings") => return Ok(ui_settings(&req, &engine, &settings, &sessions).await),
        (&Method::GET, p) if p.starts_with("/p/") => {
            let rest = p.trim_start_matches("/p/");
            // Only the bare project page here; the logs sub-path (added in
            // Task 9) is matched by its own, earlier arm.
            if !rest.is_empty() && !rest.contains('/') {
                let project = ui::html_escape(rest); // decode-safe: names are label-derived
                return Ok(ui_project(&req, &engine, &settings, &sessions, &project).await);
            }
        }
```

> Keep the existing `/login`, `/logout`, `/ui/destroy/`, `/ui/projects/` arms unchanged.

- [ ] **Step 4: Retarget POST redirects to the project page**

In `ui_projects` and `ui_destroy`, the handlers currently `redirect("/")`. Change each success redirect to the originating project's page. `ui_projects` already parses `project` in each branch — replace `redirect("/")` with `redirect(&format!("/p/{project}"))` in the `/vars`, `/registry`, `/registry/delete`, and `/delete` branches (for the `/delete` branch that deletes a whole project, redirect to `/`). For `ui_destroy`, the handler only knows the branch, not the project; redirect to `/` (Overview) after teardown.

Concretely, in `ui_projects`:
- `/vars` success: `return Ok-equivalent` → `redirect(&format!("/p/{project}"))`
- `/registry/delete`: `redirect(&format!("/p/{project}"))`
- `/registry` success: `redirect(&format!("/p/{project}"))`
- `/vars/<key>/delete` (inside `/delete` block, `project` from `split_once`): `redirect(&format!("/p/{project}"))`
- whole-project `delete`: `redirect("/")`

In `ui_destroy`, keep `redirect("/")`.

- [ ] **Step 5: Remove the old module**

In `src/lib.rs`, delete the line `pub mod dashboard;`. Then:

```bash
git rm src/dashboard.rs
```

- [ ] **Step 6: Update existing API tests that assert `redirect("/")`**

Some `api.rs` tests assert the `Location` header equals `/` after a var/registry POST. Update those to `"/p/<project>"` matching the project used in the test (e.g. the `ui_projects_registry_sets_the_credential` test posts to `/ui/projects/myproj/registry`, so its redirect is now `/p/myproj`). Search the test module for `LOCATION` / `"/"` assertions on UI POSTs and adjust. Leave the whole-project delete and destroy assertions at `/`.

- [ ] **Step 7: Build and run the full suite**

Run: `cargo test`
Expected: PASS. Fix any remaining `dashboard::` references the compiler reports (there should be none outside `api.rs`).

- [ ] **Step 8: Clippy, format, commit**

```bash
cargo clippy --all-targets && cargo fmt
git add -A
git commit -m "feat(api): serve overview/project/settings routes; retire dashboard.rs"
```

---

### Task 9: SSE live-log endpoint

**Files:**
- Modify: `src/api.rs` (add the streaming handler + route arm + tests)

**Interfaces:**
- Consumes: `Engine::service_logs` (Task 3), `ApiBody`/`BoxError`/`full` (Task 1), `StreamBody`, `hyper::body::Frame`, `futures_util::StreamExt`.
- Produces: route `GET /p/{project}/logs/{branch}/{service}` → `Response<ApiBody>` (`text/event-stream`).

- [ ] **Step 1: Write the failing tests**

In `src/api.rs`'s cookie-auth UI test module, add (using the existing `call_with_cookie` helper that drives `handle_api`):

```rust
#[tokio::test]
async fn logs_endpoint_streams_event_stream_when_authenticated() {
    let (engine, settings, sessions) = ui_test_fixtures().await; // see note
    // deploy a branch so the service container exists
    engine.deploy(request("b1", TWO_SERVICE)).await.unwrap();
    let res = call_with_cookie(
        &engine, &settings, &sessions,
        Method::GET, "/p/p/logs/b1/backend", "",
    ).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/event-stream",
    );
    let body = body_string(res).await;
    assert!(body.contains("data:"));
}

#[tokio::test]
async fn logs_endpoint_requires_authentication() {
    let (engine, settings, sessions) = ui_test_fixtures().await;
    let res = call(&engine, &settings, &sessions, Method::GET, "/p/p/logs/b1/backend", "").await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
```

> Notes on test scaffolding: reuse the existing helpers in the `api.rs` test module. `call` sends no cookie; `call_with_cookie` sends a valid session. If there is no `ui_test_fixtures()` helper, construct the engine/settings/sessions inline exactly as the neighbouring cookie-auth tests do (they already build a `FakeRuntime`-backed `Engine`, a `Settings` with a dashboard password, and `Sessions`). `TWO_SERVICE` / `request(...)` mirror engine tests — if not in scope here, deploy via the same `FakeRuntime` construction the other UI tests use, or assert against a branch/service the fixture already has. The essential assertions are the three: 200, `text/event-stream`, body contains `data:`, and 401 unauthenticated.

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test logs_endpoint_`
Expected: FAIL — route returns 404 / not found.

- [ ] **Step 3: Add the SSE handler**

In `src/api.rs`, add imports at the top: `use hyper::body::Frame;`. Then add:

```rust
/// Parse `/p/<project>/logs/<branch>/<service>` into its three segments.
fn parse_logs_path(path: &str) -> Option<(String, String, String)> {
    let rest = path.strip_prefix("/p/")?;
    let (project, tail) = rest.split_once("/logs/")?;
    let (branch, service) = tail.split_once('/')?;
    if project.is_empty() || branch.is_empty() || service.is_empty()
        || project.contains('/') || branch.contains('/') || service.contains('/')
    {
        return None;
    }
    Some((project.to_string(), branch.to_string(), service.to_string()))
}

/// `GET /p/<project>/logs/<branch>/<service>` — stream the service's container
/// logs as Server-Sent Events. Cookie-authenticated; unauthenticated requests
/// get 401 (EventSource cannot follow a login redirect).
async fn ui_logs<R: ContainerRuntime>(
    req: &Request<Incoming>,
    engine: &Engine<R>,
    settings: &Settings,
    sessions: &Sessions,
    project: &str,
    branch: &str,
    service: &str,
) -> Response<ApiBody> {
    if !dashboard_enabled(settings) {
        return text(StatusCode::SERVICE_UNAVAILABLE, "dashboard not configured");
    }
    if !session_of(req, sessions) {
        return text(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let stream = match engine.service_logs(project, branch, service, true, 200).await {
        Ok(s) => s,
        Err(_) => return text(StatusCode::NOT_FOUND, "no such service"),
    };
    // Each log line becomes one SSE frame. Newlines within a line would break
    // the framing, so any embedded newline splits into its own data field.
    let frames = stream.map(|item| -> Result<Frame<Bytes>, BoxError> {
        let line = item.map_err(|e| -> BoxError { e.into() })?;
        let payload = line
            .split('\n')
            .map(|l| format!("data: {l}\n"))
            .collect::<String>();
        Ok(Frame::data(Bytes::from(format!("{payload}\n"))))
    });
    let body = StreamBody::new(frames).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body)
        .expect("sse response is always valid")
}
```

Add `use futures_util::StreamExt;` at the top of `src/api.rs` if not already imported (needed for `.map` on the stream).

- [ ] **Step 4: Add the route arm (before the bare `/p/` project arm)**

In `handle_api`'s UI match block, add this arm **above** the `p.starts_with("/p/")` project arm from Task 8 so the logs sub-path is matched first:

```rust
        (&Method::GET, p) if parse_logs_path(p).is_some() => {
            let (project, branch, service) = parse_logs_path(p).unwrap();
            return Ok(ui_logs(&req, &engine, &settings, &sessions, &project, &branch, &service).await);
        }
```

- [ ] **Step 5: Run the tests — expect PASS**

Run: `cargo test logs_endpoint_`
Expected: PASS (both). The `FakeRuntime` canned stream provides the `data:` lines.

- [ ] **Step 6: Full suite, clippy, format, commit**

```bash
cargo test && cargo clippy --all-targets && cargo fmt
git add src/api.rs
git commit -m "feat(api): live-log SSE endpoint at /p/{project}/logs/{branch}/{service}"
```

---

### Task 10: End-to-end verification

**Files:** none (verification only)

- [ ] **Step 1: Full build, test, lint**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all PASS / clean.

- [ ] **Step 2: Manual smoke via the run/verify skill**

Invoke the `verify` (or `run`) skill to launch the server with a dashboard password set and a `FakeRuntime`/local Docker, then:
- Load `/login`, sign in.
- Confirm the left rail lists Overview, projects, Settings, Sign out; the active item highlights per route.
- Open a project page; expand a service's **live logs** panel and confirm lines stream in and auto-scroll, and that collapsing stops the stream (check no further network activity).
- Open `/settings`; confirm system info shows and the token/password do not appear in page source.
- Add and delete an env var; confirm the redirect lands back on `/p/{project}`.

- [ ] **Step 3: Commit any fixes found during smoke testing**

```bash
git add -A && git commit -m "fix: address issues found in dashboard smoke test"
```

---

## Self-Review

**Spec coverage:**
- Master-detail layout + left rail (Overview / projects / Settings / Sign out) — Tasks 4, 8. ✓
- Real server-rendered routes `/`, `/p/{project}`, `/settings` — Task 8. ✓
- Redirects retargeted to `/p/{project}` — Task 8. ✓
- Overview aggregate + cross-project list — Task 5. ✓
- Project view (deployments, env masked, registry masked) — Task 6. ✓
- Settings read-only, secrets hidden — Task 7. ✓
- Live logs: runtime `logs` (Docker + fake) — Task 2; engine resolver — Task 3; SSE endpoint — Task 9; client `EventSource` script — Task 6. ✓
- Boxed body enabling streaming — Task 1. ✓
- `ui/` module split — Tasks 4–7. ✓
- Visual refinement (tokens preserved, sidebar styling) — Tasks 4 (style.rs). Executor may apply `frontend-design` for further polish; structural CSS is provided so no placeholder remains. ✓
- Testing per view + SSE + auth — Tasks 5–9. ✓

**Placeholder scan:** The only intentional "paste the existing block" is in Task 4 Step 3 (porting the current `STYLE` verbatim) — it names exact source lines (dashboard.rs 24–152) and is a mechanical copy, not undefined work. No TBD/TODO remain.

**Type consistency:** `LogStream` alias defined in Task 2 and imported by Tasks 2/3/9. `ApiBody`/`BoxError`/`full` defined in Task 1 and used in Task 9. `status_word` defined `pub(crate)` in `overview.rs` (Task 5) and consumed by `project.rs` (Task 6). `Nav` enum and `app_shell` signature defined in Task 4 and used by `mod.rs` entry points. Handler names (`ui_overview`, `ui_project`, `ui_settings`, `ui_logs`) consistent between definition and route arms. `MaskedProject`/`MaskedVar`/`MaskedRegistry` field names match `secrets.rs` as used in existing tests.
