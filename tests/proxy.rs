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
