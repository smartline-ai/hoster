use std::sync::Arc;

use hoster::api::serve_api;
use hoster::engine::{AlwaysReady, Engine};
use hoster::routing::{RoutingTable, SharedRoutes};
use hoster::runtime::FakeRuntime;
use hoster::settings::Settings;
use tokio::net::TcpListener;

async fn spawn() -> (String, Arc<FakeRuntime>) {
    let rt = Arc::new(FakeRuntime::new());
    let settings = Arc::new(Settings {
        listen: "127.0.0.1:0".into(),
        api_listen: "127.0.0.1:0".into(),
        hostname_template: "{service}-{branch}.dev.example.com".into(),
        registry: "reg.example.com".into(),
        token: "secret".into(),
    });
    let engine = Arc::new(Engine::with_readiness(
        rt.clone(),
        SharedRoutes::new(RoutingTable::new()),
        settings.clone(),
        Arc::new(AlwaysReady),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { serve_api(listener, engine, settings).await });
    (format!("http://{addr}"), rt)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

const BODY: &str = r#"{"branch":"feature/JIRA-1","tag":"abc","sha":"sha","config":{"project":"p","services":{"backend":{"image":"{{registry}}/backend:{{tag}}","expose":{"port":8080}}}}}"#;

#[tokio::test]
async fn deploy_requires_token() {
    let (base, _) = spawn().await;
    let resp = client()
        .post(format!("{base}/deploy"))
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn deploy_happy_path_returns_202_and_urls() {
    let (base, rt) = spawn().await;
    let resp = client()
        .post(format!("{base}/deploy"))
        .bearer_auth("secret")
        .body(BODY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        json["urls"]["backend"],
        "http://backend-feature-jira-1.dev.example.com"
    );
    assert_eq!(rt.container_count(), 1);
}

#[tokio::test]
async fn invalid_config_is_400() {
    let (base, _) = spawn().await;
    let bad = r#"{"branch":"b","tag":"t","sha":"s","config":{"project":"p","services":{}}}"#;
    let resp = client()
        .post(format!("{base}/deploy"))
        .bearer_auth("secret")
        .body(bad)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn delete_is_idempotent() {
    let (base, _) = spawn().await;
    let resp = client()
        .delete(format!("{base}/deploy/does-not-exist"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn deployments_lists_after_deploy() {
    let (base, _) = spawn().await;
    client()
        .post(format!("{base}/deploy"))
        .bearer_auth("secret")
        .body(BODY)
        .send()
        .await
        .unwrap();
    let resp = client()
        .get(format!("{base}/deployments"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(
        json.as_array()
            .unwrap()
            .iter()
            .any(|d| d["branch"] == "feature-jira-1")
    );
}

#[tokio::test]
async fn healthz_is_open() {
    let (base, _) = spawn().await;
    let resp = client()
        .get(format!("{base}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
