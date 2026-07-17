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
