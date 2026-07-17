# Proxy Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A working HTTP reverse proxy that routes requests to upstream servers based on the `Host` header, driven by a hot-swappable in-memory routing table.

**Architecture:** One `tokio` task per connection, serving hyper HTTP/1.1. Every request reads an `ArcSwap<RoutingTable>` — a lock-free pointer read — maps `Host` to an upstream `SocketAddr`, and streams the request there with a hyper client. The routing table is replaced by swapping the pointer; readers never block and in-flight requests finish against the old table. This plan builds the proxy against a static routes file; a later plan replaces that file with the deploy engine, which is why the proxy knows nothing about branches, containers, or certificates.

**Tech Stack:** Rust 2024 edition (toolchain 1.93.1), `tokio`, `hyper` 1.x, `hyper-util`, `http-body-util`, `arc-swap`, `serde` + `toml`, `tracing`. Tests use `tokio-tungstenite` for the websocket case.

## Global Constraints

These come from `docs/superpowers/specs/2026-07-17-hoster-design.md`. Every task's requirements implicitly include this section.

- **Easy to operate, boring enough to trust.** When a clever mechanism competes with an obvious one, take the obvious one.
- **The proxy knows nothing about branches, containers, certificates, or deployments.** It has a map of hostnames to sockets. Anything branch-shaped in this crate is a bug. This boundary is what makes the proxy testable without containerd.
- **The routing table is `ArcSwap<RoutingTable>`.** Deploys swap the pointer. No locks on the read path.
- **Default-closed.** An unknown host is a 404, never a fallback, never a default upstream.
- **HTTP only.** No TCP proxying. Public routing works by reading the `Host` header; raw TCP has no `Host` header.
- **Host header lookups are case-insensitive, port-stripped, and trailing-dot-stripped.** DNS names are case-insensitive and `Host` may legally carry `:443` or a fully-qualified trailing dot.
- **TLS is not in this plan.** Listener is plain HTTP. `rustls` and ACME arrive in a later plan and must not leak into these modules.
- Crate name is `hoster`, already declared in `Cargo.toml` with `edition = "2024"`.

---

## File Structure

| File | Responsibility |
| --- | --- |
| `Cargo.toml` | Dependencies. |
| `src/main.rs` | Binary entry. Loads config, builds the table, starts the server. Wiring only — no logic. |
| `src/lib.rs` | Crate root. Declares modules so integration tests can use them. |
| `src/routing.rs` | `Route`, `RouteState`, `RoutingTable`, `SharedRoutes`, host normalization. No HTTP types. |
| `src/proxy.rs` | `serve()` and `handle()`. The request path. No knowledge of where routes come from. |
| `src/routes_file.rs` | Parses the static TOML routes file into a `RoutingTable`. Temporary — the deploy engine replaces it in a later plan. |
| `tests/support/mod.rs` | Test-only stub upstream server. |
| `tests/proxy.rs` | Integration tests for the request path. |

`routing.rs` holds no HTTP types and `proxy.rs` holds no route-construction logic, so each is testable alone. `routes_file.rs` is isolated precisely because it is the throwaway part.

---

### Task 1: Routing table

**Files:**
- Modify: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/routing.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `hoster::routing::RouteState` — `enum { Starting, Ready }`, derives `Debug, Clone, Copy, PartialEq, Eq`.
  - `hoster::routing::Route` — `struct { pub upstream: std::net::SocketAddr, pub state: RouteState }`, derives `Debug, Clone, PartialEq, Eq`.
  - `hoster::routing::RoutingTable` — `RoutingTable::new() -> Self`, `insert(&mut self, host: impl Into<String>, route: Route)`, `lookup(&self, host: &str) -> Option<&Route>`, `len(&self) -> usize`, `is_empty(&self) -> bool`. Derives `Debug, Default`.
  - `hoster::routing::SharedRoutes` — `SharedRoutes::new(RoutingTable) -> Self`, `load(&self) -> arc_swap::Guard<std::sync::Arc<RoutingTable>>`, `swap(&self, RoutingTable)`. Derives `Clone`.
  - `hoster::routing::normalize_host(host: &str) -> String`.

- [ ] **Step 1: Add dependencies**

Replace the `[dependencies]` section of `Cargo.toml` (keep the existing `[package]` section exactly as it is):

```toml
[dependencies]
anyhow = "1"
arc-swap = "1"
bytes = "1"
http-body-util = "0.1"
hyper = { version = "1", features = ["server", "client", "http1"] }
hyper-util = { version = "0.1", features = ["tokio", "server", "client", "client-legacy", "http1"] }
serde = { version = "1", features = ["derive"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "net", "io-util", "signal"] }
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

[dev-dependencies]
futures-util = "0.3"
reqwest = { version = "0.12", default-features = false }
tokio-tungstenite = "0.24"
```

- [ ] **Step 2: Write the failing test**

Create `src/routing.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn route(port: u16) -> Route {
        Route {
            upstream: format!("127.0.0.1:{port}").parse().unwrap(),
            state: RouteState::Ready,
        }
    }

    #[test]
    fn normalizes_case() {
        assert_eq!(normalize_host("Backend-Foo.Dev.Example.Com"), "backend-foo.dev.example.com");
    }

    #[test]
    fn normalizes_strips_port() {
        assert_eq!(normalize_host("backend-foo.dev.example.com:443"), "backend-foo.dev.example.com");
    }

    #[test]
    fn normalizes_strips_trailing_dot() {
        assert_eq!(normalize_host("backend-foo.dev.example.com."), "backend-foo.dev.example.com");
    }

    #[test]
    fn lookup_finds_inserted_route() {
        let mut table = RoutingTable::new();
        table.insert("backend-foo.dev.example.com", route(8080));
        assert_eq!(table.lookup("backend-foo.dev.example.com"), Some(&route(8080)));
    }

    #[test]
    fn lookup_normalizes_the_query() {
        let mut table = RoutingTable::new();
        table.insert("backend-foo.dev.example.com", route(8080));
        assert_eq!(table.lookup("Backend-Foo.dev.example.com:443"), Some(&route(8080)));
    }

    #[test]
    fn lookup_normalizes_the_insert() {
        let mut table = RoutingTable::new();
        table.insert("Backend-Foo.Dev.Example.Com.", route(8080));
        assert_eq!(table.lookup("backend-foo.dev.example.com"), Some(&route(8080)));
    }

    #[test]
    fn unknown_host_is_none() {
        let mut table = RoutingTable::new();
        table.insert("backend-foo.dev.example.com", route(8080));
        assert_eq!(table.lookup("backend-bar.dev.example.com"), None);
    }

    #[test]
    fn two_branches_same_port_do_not_collide() {
        let mut table = RoutingTable::new();
        table.insert("backend-branch1.dev.example.com", route(8080));
        table.insert("backend-branch2.dev.example.com", route(8080));
        assert_eq!(table.lookup("backend-branch1.dev.example.com").unwrap().upstream.port(), 8080);
        assert_eq!(table.lookup("backend-branch2.dev.example.com").unwrap().upstream.port(), 8080);
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn swap_replaces_the_whole_table() {
        let mut first = RoutingTable::new();
        first.insert("a.example.com", route(1));
        let shared = SharedRoutes::new(first);
        assert!(shared.load().lookup("a.example.com").is_some());

        let mut second = RoutingTable::new();
        second.insert("b.example.com", route(2));
        shared.swap(second);

        assert!(shared.load().lookup("a.example.com").is_none());
        assert!(shared.load().lookup("b.example.com").is_some());
    }
}
```

Create `src/lib.rs`:

```rust
pub mod routing;
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib`
Expected: FAIL to compile, with errors like `cannot find type Route in this scope` and `cannot find function normalize_host in this scope`.

- [ ] **Step 4: Write the implementation**

Add this to the **top** of `src/routing.rs`, above the existing `#[cfg(test)] mod tests`:

```rust
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;

/// Whether a route is ready to receive traffic.
///
/// `Starting` exists so a branch URL can answer "starting" instead of refusing
/// the connection while its containers boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteState {
    Starting,
    Ready,
}

/// Where one hostname's traffic goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub upstream: SocketAddr,
    pub state: RouteState,
}

/// An immutable snapshot of every live hostname.
///
/// Built whole and swapped in; never mutated while readers hold it.
#[derive(Debug, Default)]
pub struct RoutingTable {
    routes: HashMap<String, Route>,
}

impl RoutingTable {
    pub fn new() -> Self {
        Self { routes: HashMap::new() }
    }

    pub fn insert(&mut self, host: impl Into<String>, route: Route) {
        self.routes.insert(normalize_host(&host.into()), route);
    }

    pub fn lookup(&self, host: &str) -> Option<&Route> {
        self.routes.get(&normalize_host(host))
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

/// Canonical form of a `Host` header for lookup.
///
/// A `Host` header may carry a port (`example.com:443`) and may be fully
/// qualified with a trailing dot. DNS names are case-insensitive. All three
/// must collapse to the same key or lookups miss for reasons nobody enjoys
/// debugging.
///
/// IPv6 literal hosts (`[::1]:80`) are not supported: hoster routes DNS names.
pub fn normalize_host(host: &str) -> String {
    host.split(':')
        .next()
        .unwrap_or(host)
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// The handoff between the deploy engine (writer) and the proxy (readers).
///
/// Readers take a pointer snapshot with no lock. The writer builds a fresh
/// table and stores it. In-flight requests finish against the table they
/// loaded, so a deploy going live is one atomic pointer swap.
#[derive(Clone)]
pub struct SharedRoutes(Arc<ArcSwap<RoutingTable>>);

impl SharedRoutes {
    pub fn new(table: RoutingTable) -> Self {
        Self(Arc::new(ArcSwap::from_pointee(table)))
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<RoutingTable>> {
        self.0.load()
    }

    pub fn swap(&self, table: RoutingTable) {
        self.0.store(Arc::new(table));
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, 9 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/routing.rs
git commit -m "feat: routing table with hot-swappable snapshots"
```

---

### Task 2: Proxy request path

**Files:**
- Create: `src/proxy.rs`
- Modify: `src/lib.rs`
- Create: `tests/support/mod.rs`
- Create: `tests/proxy.rs`

**Interfaces:**
- Consumes: `hoster::routing::{Route, RouteState, RoutingTable, SharedRoutes}` from Task 1.
- Produces:
  - `hoster::proxy::HttpClient` — type alias for the upstream client.
  - `hoster::proxy::build_client() -> HttpClient`.
  - `hoster::proxy::serve(listener: tokio::net::TcpListener, routes: SharedRoutes) -> anyhow::Result<()>` — runs until the process ends.
  - `hoster::proxy::handle(req: hyper::Request<hyper::body::Incoming>, routes: SharedRoutes, client: HttpClient) -> Result<hyper::Response<ProxyBody>, std::convert::Infallible>`.
  - `hoster::proxy::ProxyBody` — `BoxBody<bytes::Bytes, Box<dyn std::error::Error + Send + Sync>>`.

- [ ] **Step 1: Write the stub upstream helper**

Create `tests/support/mod.rs`. This is a real hyper server on an ephemeral port that echoes back what it received, so tests can assert what the proxy forwarded.

```rust
// Each integration test binary compiles this module separately and uses only
// part of it, so unused-code warnings here are structural, not real.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

/// What the stub upstream saw on its most recent request.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub host: Option<String>,
    pub path: Option<String>,
    pub forwarded_host: Option<String>,
    pub forwarded_proto: Option<String>,
}

pub struct Upstream {
    pub addr: SocketAddr,
    pub seen: Arc<Mutex<Seen>>,
}

/// Spawns an upstream that replies `200 OK` with `body` and records the
/// request it received.
pub async fn spawn_upstream(body: &'static str) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
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
                        let header = |name: &str| {
                            req.headers()
                                .get(name)
                                .and_then(|v| v.to_str().ok())
                                .map(str::to_string)
                        };
                        *seen.lock().unwrap() = Seen {
                            host: header("host"),
                            path: Some(req.uri().path().to_string()),
                            forwarded_host: header("x-forwarded-host"),
                            forwarded_proto: header("x-forwarded-proto"),
                        };
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .body(Full::new(Bytes::from(body)))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });

    Upstream { addr, seen }
}
```

- [ ] **Step 2: Write the failing test**

Create `tests/proxy.rs`:

```rust
mod support;

use hoster::proxy::serve;
use hoster::routing::{Route, RouteState, RoutingTable, SharedRoutes};
use tokio::net::TcpListener;

/// Starts the proxy on an ephemeral port. Returns its base URL.
async fn spawn_proxy(routes: SharedRoutes) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(listener, routes).await;
    });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().unwrap()
}

#[tokio::test]
async fn proxies_to_the_upstream_for_a_known_host() {
    let upstream = support::spawn_upstream("hello from branch1").await;
    let mut table = RoutingTable::new();
    table.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: upstream.addr, state: RouteState::Ready },
    );
    let base = spawn_proxy(SharedRoutes::new(table)).await;

    let resp = client()
        .get(format!("{base}/some/path"))
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello from branch1");

    let seen = upstream.seen.lock().unwrap().clone();
    assert_eq!(seen.path.as_deref(), Some("/some/path"));
}

#[tokio::test]
async fn two_hosts_on_the_same_upstream_port_reach_different_branches() {
    // The collision question from the design: both upstreams listen on their
    // own port here, but the point is that one proxy serves both hostnames.
    let branch1 = support::spawn_upstream("one").await;
    let branch2 = support::spawn_upstream("two").await;
    let mut table = RoutingTable::new();
    table.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: branch1.addr, state: RouteState::Ready },
    );
    table.insert(
        "backend-branch2.dev.example.com",
        Route { upstream: branch2.addr, state: RouteState::Ready },
    );
    let base = spawn_proxy(SharedRoutes::new(table)).await;

    let one = client()
        .get(&base)
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let two = client()
        .get(&base)
        .header("host", "backend-branch2.dev.example.com")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(one, "one");
    assert_eq!(two, "two");
}

#[tokio::test]
async fn preserves_the_original_host_header() {
    let upstream = support::spawn_upstream("ok").await;
    let mut table = RoutingTable::new();
    table.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: upstream.addr, state: RouteState::Ready },
    );
    let base = spawn_proxy(SharedRoutes::new(table)).await;

    client()
        .get(&base)
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap();

    let seen = upstream.seen.lock().unwrap().clone();
    assert_eq!(seen.host.as_deref(), Some("backend-branch1.dev.example.com"));
    assert_eq!(seen.forwarded_host.as_deref(), Some("backend-branch1.dev.example.com"));
    assert_eq!(seen.forwarded_proto.as_deref(), Some("http"));
}

#[tokio::test]
async fn unknown_host_is_404() {
    let base = spawn_proxy(SharedRoutes::new(RoutingTable::new())).await;

    let resp = client()
        .get(&base)
        .header("host", "nope.dev.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn starting_route_is_503() {
    let upstream = support::spawn_upstream("should not be reached").await;
    let mut table = RoutingTable::new();
    table.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: upstream.addr, state: RouteState::Starting },
    );
    let base = spawn_proxy(SharedRoutes::new(table)).await;

    let resp = client()
        .get(&base)
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    assert!(resp.text().await.unwrap().contains("starting"));
}

#[tokio::test]
async fn dead_upstream_is_502() {
    // Bind then drop, so the port is almost certainly closed.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);

    let mut table = RoutingTable::new();
    table.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: dead_addr, state: RouteState::Ready },
    );
    let base = spawn_proxy(SharedRoutes::new(table)).await;

    let resp = client()
        .get(&base)
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 502);
}

#[tokio::test]
async fn swapping_the_table_changes_routing_without_restart() {
    let first = support::spawn_upstream("first").await;
    let second = support::spawn_upstream("second").await;

    let mut table = RoutingTable::new();
    table.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: first.addr, state: RouteState::Ready },
    );
    let routes = SharedRoutes::new(table);
    let base = spawn_proxy(routes.clone()).await;

    let before = client()
        .get(&base)
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(before, "first");

    let mut next = RoutingTable::new();
    next.insert(
        "backend-branch1.dev.example.com",
        Route { upstream: second.addr, state: RouteState::Ready },
    );
    routes.swap(next);

    let after = client()
        .get(&base)
        .header("host", "backend-branch1.dev.example.com")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(after, "second");
}
```

Add the module to `src/lib.rs`:

```rust
pub mod proxy;
pub mod routing;
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --test proxy`
Expected: FAIL to compile, `unresolved import hoster::proxy`.

- [ ] **Step 4: Write the implementation**

Create `src/proxy.rs`:

```rust
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --test proxy`
Expected: PASS, 7 tests.

If `preserves_the_original_host_header` fails with the upstream's own socket address as `host`, the hyper client overrode the header from the URI authority. Fix by re-inserting the header immediately before `client.request(req)` rather than earlier, and re-run.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/proxy.rs tests/support/mod.rs tests/proxy.rs
git commit -m "feat: proxy requests to upstreams by Host header"
```

---

### Task 3: Websocket upgrade passthrough

**Files:**
- Modify: `src/proxy.rs`
- Modify: `tests/support/mod.rs`
- Create: `tests/websocket.rs`

**Interfaces:**
- Consumes: `hoster::proxy::{serve, ProxyBody}`, `hoster::routing::*`.
- Produces: no new public names. `handle` gains upgrade behavior.

Frontend dev servers push rebuilds over websockets. Without this, every branch's dev server appears broken in a way that looks like hoster's fault.

- [ ] **Step 1: Add a websocket echo upstream to the test support**

Append to `tests/support/mod.rs`:

```rust
use futures_util::{SinkExt, StreamExt};

/// Spawns an upstream that accepts a websocket handshake and echoes text
/// frames back with an `echo: ` prefix.
pub async fn spawn_ws_upstream() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            tokio::spawn(async move {
                let mut ws = match tokio_tungstenite::accept_async(stream).await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                while let Some(Ok(msg)) = ws.next().await {
                    if msg.is_text() {
                        let reply = format!("echo: {}", msg.into_text().unwrap());
                        if ws
                            .send(tokio_tungstenite::tungstenite::Message::Text(reply.into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });

    addr
}
```

- [ ] **Step 2: Write the failing test**

Create `tests/websocket.rs`:

```rust
mod support;

use futures_util::{SinkExt, StreamExt};
use hoster::proxy::serve;
use hoster::routing::{Route, RouteState, RoutingTable, SharedRoutes};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn proxies_a_websocket_conversation() {
    let upstream = support::spawn_ws_upstream().await;

    let mut table = RoutingTable::new();
    table.insert(
        "frontend-branch1.dev.example.com",
        Route { upstream, state: RouteState::Ready },
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(listener, SharedRoutes::new(table)).await;
    });

    // Connect to the proxy, but claim the branch hostname so it routes.
    let mut request = format!("ws://{proxy_addr}/socket").into_client_request().unwrap();
    request.headers_mut().insert(
        "host",
        "frontend-branch1.dev.example.com".parse().unwrap(),
    );

    let (mut ws, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.status(), 101);

    ws.send(Message::Text("hello".into())).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    assert_eq!(reply.into_text().unwrap().as_str(), "echo: hello");
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --test websocket`
Expected: FAIL — the handshake does not complete. The proxy currently forwards the request as a normal one, so no 101 reaches the client and `connect_async` errors.

- [ ] **Step 4: Write the implementation**

In `src/proxy.rs`, add to the imports:

```rust
use hyper::header::{CONNECTION, UPGRADE};
```

Add this helper below `text()`:

```rust
/// True when the client asked to leave HTTP behind (websockets, mostly).
///
/// `Connection` is a comma-separated list and both header values are
/// case-insensitive, so this cannot be an equality check.
fn is_upgrade_request(req: &Request<Incoming>) -> bool {
    let connection_has_upgrade = req
        .headers()
        .get(CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")))
        .unwrap_or(false);

    connection_has_upgrade && req.headers().contains_key(UPGRADE)
}
```

In `handle`, replace the final `match client.request(req).await { ... }` block with:

```rust
    let upgrading = is_upgrade_request(&req);

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
            let (client_io, upstream_io) = match tokio::try_join!(client_upgrade, upstream_upgrade) {
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
            if let Err(e) =
                tokio::io::copy_bidirectional(&mut client_io, &mut upstream_io).await
            {
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS — 9 lib tests, 7 proxy tests, 1 websocket test. The Task 2 tests must still pass; ordinary requests take the same path as before.

- [ ] **Step 6: Commit**

```bash
git add src/proxy.rs tests/support/mod.rs tests/websocket.rs
git commit -m "feat: pass websocket upgrades through to containers"
```

---

### Task 4: Static routes file and binary wiring

**Files:**
- Create: `src/routes_file.rs`
- Modify: `src/lib.rs`
- Modify: `src/main.rs`
- Create: `routes.example.toml`

**Interfaces:**
- Consumes: `hoster::routing::{Route, RouteState, RoutingTable, SharedRoutes}`, `hoster::proxy::serve`.
- Produces:
  - `hoster::routes_file::RoutesFile` — `struct { pub routes: Vec<RouteEntry> }`, derives `Debug, serde::Deserialize`.
  - `hoster::routes_file::RouteEntry` — `struct { pub host: String, pub upstream: String, pub starting: bool }`, derives `Debug, serde::Deserialize`.
  - `hoster::routes_file::parse(toml_text: &str) -> anyhow::Result<RoutingTable>`.

This file is scaffolding. The deploy engine builds `RoutingTable` directly in a later plan and this module is deleted. It exists so the proxy is runnable and demonstrable now.

- [ ] **Step 1: Write the failing test**

Create `src/routes_file.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::RouteState;

    #[test]
    fn parses_a_ready_route() {
        let table = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"
            "#,
        )
        .unwrap();

        let route = table.lookup("backend-branch1.dev.example.com").unwrap();
        assert_eq!(route.upstream.to_string(), "127.0.0.1:8080");
        assert_eq!(route.state, RouteState::Ready);
    }

    #[test]
    fn starting_flag_maps_to_starting_state() {
        let table = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"
            starting = true
            "#,
        )
        .unwrap();

        assert_eq!(
            table.lookup("backend-branch1.dev.example.com").unwrap().state,
            RouteState::Starting
        );
    }

    #[test]
    fn parses_many_routes() {
        let table = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"

            [[routes]]
            host = "frontend-branch1.dev.example.com"
            upstream = "127.0.0.1:3000"
            "#,
        )
        .unwrap();

        assert_eq!(table.len(), 2);
    }

    #[test]
    fn empty_file_is_an_empty_table() {
        let table = parse("").unwrap();
        assert!(table.is_empty());
    }

    #[test]
    fn bad_upstream_is_an_error_naming_the_host() {
        let err = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "not-a-socket-address"
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("backend-branch1.dev.example.com"), "got: {err}");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"
            tls = true
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("tls"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib`
Expected: FAIL to compile, `cannot find function parse in this scope`.

- [ ] **Step 3: Write the implementation**

Add to the top of `src/routes_file.rs`:

```rust
//! Static routes file.
//!
//! Scaffolding so the proxy is runnable before the deploy engine exists.
//! The deploy engine builds `RoutingTable` directly; this module is deleted
//! when it lands.

use anyhow::Context;
use serde::Deserialize;

use crate::routing::{Route, RouteState, RoutingTable};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutesFile {
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub host: String,
    pub upstream: String,
    #[serde(default)]
    pub starting: bool,
}

pub fn parse(toml_text: &str) -> anyhow::Result<RoutingTable> {
    let file: RoutesFile = toml::from_str(toml_text).context("routes file is not valid TOML")?;

    let mut table = RoutingTable::new();
    for entry in file.routes {
        let upstream = entry
            .upstream
            .parse()
            .with_context(|| format!("route {}: upstream {:?} is not a host:port address", entry.host, entry.upstream))?;
        let state = if entry.starting { RouteState::Starting } else { RouteState::Ready };
        table.insert(entry.host, Route { upstream, state });
    }
    Ok(table)
}
```

Update `src/lib.rs`:

```rust
pub mod proxy;
pub mod routes_file;
pub mod routing;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, 15 tests.

- [ ] **Step 5: Wire up the binary**

Replace the whole of `src/main.rs`:

```rust
use std::path::PathBuf;

use anyhow::Context;
use hoster::proxy::serve;
use hoster::routing::SharedRoutes;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hoster=info".into()),
        )
        .init();

    let routes_path: PathBuf = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "routes.toml".to_string())
        .into();
    let listen = std::env::var("HOSTER_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let text = std::fs::read_to_string(&routes_path)
        .with_context(|| format!("could not read routes file {}", routes_path.display()))?;
    let table = hoster::routes_file::parse(&text)?;
    tracing::info!(routes = table.len(), path = %routes_path.display(), "loaded routes");

    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("could not bind {listen}"))?;

    serve(listener, SharedRoutes::new(table)).await
}
```

Create `routes.example.toml`:

```toml
# Static routes for local development.
#
# Temporary: the deploy engine will build this table from containerd instead.
# Run:  cargo run -- routes.example.toml
# Then: curl -H 'Host: backend-branch1.dev.example.com' http://127.0.0.1:8080/

[[routes]]
host = "backend-branch1.dev.example.com"
upstream = "127.0.0.1:9001"

[[routes]]
host = "backend-branch2.dev.example.com"
upstream = "127.0.0.1:9002"

# A route whose containers are still booting answers "starting", not a
# connection error.
[[routes]]
host = "frontend-branch1.dev.example.com"
upstream = "127.0.0.1:9003"
starting = true
```

- [ ] **Step 6: Verify it runs against a real upstream**

In one terminal, serve anything on 9001:

```bash
mkdir -p /tmp/hoster-demo && echo 'branch1 backend' > /tmp/hoster-demo/index.html
python3 -m http.server 9001 --directory /tmp/hoster-demo
```

In a second terminal:

```bash
cargo run -- routes.example.toml
```

In a third:

```bash
curl -s -H 'Host: backend-branch1.dev.example.com' http://127.0.0.1:8080/
curl -s -o /dev/null -w '%{http_code}\n' -H 'Host: frontend-branch1.dev.example.com' http://127.0.0.1:8080/
curl -s -o /dev/null -w '%{http_code}\n' -H 'Host: nope.dev.example.com' http://127.0.0.1:8080/
curl -s -o /dev/null -w '%{http_code}\n' -H 'Host: backend-branch2.dev.example.com' http://127.0.0.1:8080/
```

Expected, in order: `branch1 backend`, then `503`, then `404`, then `502`.

Stop the python server and the proxy.

- [ ] **Step 7: Run the whole suite and lint**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all tests PASS, no clippy warnings, formatting clean.

If `cargo fmt --check` reports diffs, run `cargo fmt` and re-run.

- [ ] **Step 8: Commit**

```bash
git add src/lib.rs src/main.rs src/routes_file.rs routes.example.toml
git commit -m "feat: run the proxy from a static routes file"
```

---

## Done when

- `cargo test` passes: routing table, request path, websockets, routes file.
- `cargo run -- routes.example.toml` serves a real upstream by `Host` header.
- Unknown host is 404, starting route is 503, dead upstream is 502.
- Nothing in `src/routing.rs` or `src/proxy.rs` mentions branches, containers, or certificates.

## Next plan

Build order steps 2–4 from the design: containerd integration, per-branch network namespaces with service-name DNS, and the deploy flow that builds a `RoutingTable` and calls `SharedRoutes::swap`. That plan deletes `src/routes_file.rs`.
