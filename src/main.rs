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
        .unwrap_or_else(|| "routes.example.toml".to_string())
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
