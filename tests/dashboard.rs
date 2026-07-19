use std::sync::Arc;

use hoster::api::serve_api;
use hoster::engine::{AlwaysReady, Engine};
use hoster::routing::{RoutingTable, SharedRoutes};
use hoster::runtime::FakeRuntime;
use hoster::secrets::Store;
use hoster::settings::Settings;
use tokio::net::TcpListener;

fn temp_store() -> Arc<Store> {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "hoster-dash-it-{}-{n}/projects.json",
        std::process::id()
    ));
    Arc::new(Store::load(path).unwrap())
}

fn settings(password: Option<&str>) -> Arc<Settings> {
    Arc::new(Settings {
        listen: "127.0.0.1:0".into(),
        api_listen: "127.0.0.1:0".into(),
        hostname_template: "{service}-{branch}.example.com".into(),
        registry: "reg.example.com".into(),
        token: "secret".into(),
        dashboard_password: password.map(str::to_string),
        https_listen: None,
        cert_dir: "/tmp/hoster-test-certs".into(),
    })
}

async fn spawn(password: Option<&str>) -> (String, Arc<FakeRuntime>) {
    let rt = Arc::new(FakeRuntime::new());
    let engine = Arc::new(Engine::with_readiness(
        rt.clone(),
        SharedRoutes::new(RoutingTable::new()),
        settings(password),
        Arc::new(AlwaysReady),
        temp_store(),
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
    assert!(
        resp.text()
            .await
            .unwrap()
            .to_lowercase()
            .contains("invalid")
    );
}

#[tokio::test]
async fn login_then_dashboard_then_destroy() {
    let (base, rt) = spawn(Some("pw")).await;
    let c = client();

    // deploy a branch via the bearer API so the dashboard has a row
    c.post(format!("{base}/deploy"))
        .bearer_auth("secret")
        .body(DEPLOY_BODY)
        .send()
        .await
        .unwrap();
    // give the spawned deploy a moment
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(rt.container_count(), 1);

    // log in — cookie is stored by the client
    let login = c
        .post(format!("{base}/login"))
        .form(&[("password", "pw")])
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), 303);
    assert!(
        login
            .headers()
            .get_all("set-cookie")
            .iter()
            .any(|v| v.to_str().unwrap().contains("hoster_session="))
    );

    // dashboard renders the branch
    let dash = c.get(&base).send().await.unwrap();
    assert_eq!(dash.status(), 200);
    let html = dash.text().await.unwrap();
    assert!(html.contains("feature-x"));

    // destroy it via the UI form
    let del = c
        .post(format!("{base}/ui/destroy/feature-x"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 303);
    assert_eq!(rt.container_count(), 0);
}

#[tokio::test]
async fn ui_set_var_appears_masked_then_deletes() {
    let (base, _) = spawn(Some("pw")).await;
    let c = client();
    c.post(format!("{base}/login"))
        .form(&[("password", "pw")])
        .send()
        .await
        .unwrap();

    // Add a variable through the dashboard form.
    let set = c
        .post(format!("{base}/ui/projects/odinvestor/vars"))
        .form(&[
            ("key", "GOOGLE_API_KEY"),
            ("value", "AIzaSECRET"),
            ("services", "backend"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(set.status(), 303);

    // Project page shows the project + key, targets, but never the value.
    let html = c
        .get(format!("{base}/p/odinvestor"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("odinvestor"));
    assert!(html.contains("GOOGLE_API_KEY"));
    assert!(html.contains("backend"));
    assert!(
        !html.contains("AIzaSECRET"),
        "secret value leaked into dashboard HTML"
    );

    // Delete it through the UI.
    let del = c
        .post(format!(
            "{base}/ui/projects/odinvestor/vars/GOOGLE_API_KEY/delete"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), 303);
    let html = c
        .get(format!("{base}/p/odinvestor"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!html.contains("GOOGLE_API_KEY"));
}

#[tokio::test]
async fn ui_set_var_requires_cookie() {
    let (base, _) = spawn(Some("pw")).await;
    // No login → the set must be rejected and nothing stored.
    let resp = client()
        .post(format!("{base}/ui/projects/p/vars"))
        .form(&[("key", "K"), ("value", "v"), ("services", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.headers()["location"], "/login");
}

#[tokio::test]
async fn destroy_without_cookie_is_rejected() {
    let (base, rt) = spawn(Some("pw")).await;
    let c = client();
    c.post(format!("{base}/deploy"))
        .bearer_auth("secret")
        .body(DEPLOY_BODY)
        .send()
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // no login → no cookie → destroy must not happen
    let resp = c
        .post(format!("{base}/ui/destroy/feature-x"))
        .send()
        .await
        .unwrap();
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
async fn empty_password_disables_dashboard() {
    let (base, _) = spawn(Some("")).await;
    let c = client();
    let get = c.get(format!("{base}/login")).send().await.unwrap();
    assert_eq!(get.status(), 503);

    let post = c
        .post(format!("{base}/login"))
        .form(&[("password", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), 503);
}

#[tokio::test]
async fn bearer_only_on_root_redirects_to_login() {
    let (base, _) = spawn(Some("pw")).await;
    let resp = client()
        .get(&base)
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303);
    assert_eq!(resp.headers()["location"], "/login");
}

#[tokio::test]
async fn bearer_api_still_works_and_ignores_cookies() {
    let (base, _) = spawn(Some("pw")).await;
    let resp = client()
        .get(format!("{base}/deployments"))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // and the cookie alone does NOT authorize the bearer route
    let no = client()
        .get(format!("{base}/deployments"))
        .send()
        .await
        .unwrap();
    assert_eq!(no.status(), 401);
}
