use std::sync::Arc;

use anyhow::Context;
use hoster::docker::DockerRuntime;
use hoster::engine::Engine;
use hoster::proxy::serve;
use hoster::readiness::NetworkReadiness;
use hoster::routing::{RoutingTable, SharedRoutes};
use hoster::settings::Settings;
use tokio::net::TcpListener;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hoster=info".into()),
        )
        .init();

    let settings = Arc::new(Settings {
        listen: env_or("HOSTER_LISTEN", "127.0.0.1:8080"),
        api_listen: env_or("HOSTER_API_LISTEN", "127.0.0.1:8081"),
        hostname_template: env_or(
            "HOSTER_HOSTNAME_TEMPLATE",
            "{service}-{branch}.dev.example.com",
        ),
        registry: env_or("HOSTER_REGISTRY", "localhost:5000"),
        token: std::env::var("HOSTER_TOKEN").context("HOSTER_TOKEN must be set")?,
    });

    let runtime = Arc::new(DockerRuntime::connect().context("connect to Docker")?);
    runtime
        .ping()
        .await
        .context("Docker daemon not reachable")?;

    let routes = SharedRoutes::new(RoutingTable::new());
    let engine = Arc::new(Engine::new(
        runtime,
        routes.clone(),
        settings.clone(),
        Arc::new(NetworkReadiness::default()),
    ));

    // Rebuild routing from any containers a previous run left behind.
    if let Err(e) = engine.reconcile().await {
        tracing::warn!(error = %e, "startup reconcile failed; starting with empty routes");
    }

    let proxy_listener = TcpListener::bind(&settings.listen)
        .await
        .with_context(|| format!("bind proxy {}", settings.listen))?;
    let api_listener = TcpListener::bind(&settings.api_listen)
        .await
        .with_context(|| format!("bind api {}", settings.api_listen))?;

    tracing::info!(proxy = %settings.listen, api = %settings.api_listen, "hoster up");

    let proxy = tokio::spawn(serve(proxy_listener, routes));
    let api = tokio::spawn(hoster::api::serve_api(api_listener, engine, settings));

    tokio::select! {
        r = proxy => r.context("proxy task panicked")?,
        r = api => r.context("api task panicked")?,
    }
}
