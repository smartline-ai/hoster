use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use hoster::acme::{CertIssuer, IssuedCert, Issuer};
use hoster::certs::CertStore;
use hoster::dns::{CloudflareProvider, DnsProvider};
use hoster::docker::DockerRuntime;
use hoster::engine::Engine;
use hoster::proxy::serve;
use hoster::readiness::NetworkReadiness;
use hoster::renewal;
use hoster::routing::{RoutingTable, SharedRoutes};
use hoster::secrets::Store;
use hoster::settings::{Settings, wildcard_base};
use hoster::tls::{CertResolver, SharedCerts};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_flag(key: &str) -> bool {
    matches!(
        std::env::var(key).unwrap_or_default().as_str(),
        "1" | "true" | "yes"
    )
}

/// An issuer that reads its ACME configuration from the store on every
/// attempt, so credentials added through the API take effect on the next
/// renewal pass rather than at the next restart. Issuance is simply skipped —
/// with an error, which the loop's backoff absorbs — until it is configured.
struct StoreIssuer {
    store: Arc<Store>,
    account_path: PathBuf,
    production: bool,
}

#[async_trait::async_trait]
impl CertIssuer for StoreIssuer {
    async fn issue(&self, domain: &str) -> anyhow::Result<IssuedCert> {
        // The token is read here and nowhere else on this path; it is handed
        // straight to the provider and never logged or returned.
        let cfg = self
            .store
            .acme_config()
            .ok_or_else(|| anyhow::anyhow!("ACME is not configured (no account email)"))?;
        let provider = cfg
            .provider
            .ok_or_else(|| anyhow::anyhow!("ACME has no DNS provider credentials configured"))?;
        if provider.kind != "cloudflare" {
            anyhow::bail!("unsupported DNS provider {:?}", provider.kind);
        }
        let dns: Arc<dyn DnsProvider> = Arc::new(CloudflareProvider::new(provider.token));
        let issuer = Issuer::new(self.account_path.clone(), cfg.email, dns);
        let issuer = if self.production {
            issuer.use_production()
        } else {
            issuer
        };
        issuer.issue_cert(domain).await
    }
}

/// Terminate TLS and hand each connection to the same proxy service the plain
/// listener uses. The `ServerConfig` is re-read per connection, so a renewal
/// pass that swaps in a new certificate is picked up without a restart.
async fn serve_https(
    listener: TcpListener,
    certs: SharedCerts,
    routes: SharedRoutes,
) -> anyhow::Result<()> {
    let client = hoster::proxy::build_client();
    tracing::info!(addr = %listener.local_addr()?, "https listening");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // A per-connection accept error must never kill the listener.
                tracing::warn!(error = %e, "https accept failed");
                continue;
            }
        };

        let keepalive = socket2::TcpKeepalive::new().with_time(std::time::Duration::from_secs(30));
        if let Err(e) = socket2::SockRef::from(&stream).set_tcp_keepalive(&keepalive) {
            tracing::debug!(error = %e, "could not set tcp keepalive");
        }

        let acceptor = tokio_rustls::TlsAcceptor::from(certs.server_config());
        let routes = routes.clone();
        let client = client.clone();
        tokio::spawn(async move {
            let tls = match acceptor.accept(stream).await {
                Ok(t) => t,
                Err(e) => {
                    // Routine: a probe, a client with no SNI, or a name we
                    // hold no certificate for. Never fatal.
                    tracing::debug!(%peer, error = %e, "tls handshake failed");
                    return;
                }
            };
            let service = service_fn(move |req| {
                hoster::proxy::handle(
                    req,
                    routes.clone(),
                    client.clone(),
                    hoster::proxy::Scheme::Https,
                )
            });
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service)
                .with_upgrades()
                .await
            {
                tracing::debug!(%peer, error = %e, "https connection closed with error");
            }
        });
    }
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
        dashboard_password: std::env::var("HOSTER_DASHBOARD_PASSWORD")
            .ok()
            .filter(|s| !s.is_empty()),
        https_listen: std::env::var("HOSTER_HTTPS_LISTEN")
            .ok()
            .filter(|s| !s.is_empty()),
        cert_dir: env_or("HOSTER_CERT_DIR", "/var/lib/hoster/certs"),
    });

    let runtime = Arc::new(DockerRuntime::connect().context("connect to Docker")?);
    if let Err(e) = runtime.ping().await {
        tracing::warn!(error = %e, "Docker daemon not reachable at startup; deploys will fail until it returns");
    }

    let projects_file = env_or("HOSTER_PROJECTS_FILE", "/etc/hoster/projects.json");
    let store = Arc::new(
        Store::load(&projects_file)
            .with_context(|| format!("load project env store {projects_file}"))?,
    );

    let routes = SharedRoutes::new(RoutingTable::new());
    let engine = Arc::new(Engine::new(
        runtime,
        routes.clone(),
        settings.clone(),
        Arc::new(NetworkReadiness::default()),
        store.clone(),
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

    // TLS is entirely opt-in. With `HOSTER_HTTPS_LISTEN` unset there is no
    // listener, no renewal loop, and no issuance, so upgrading an existing
    // install changes nothing.
    let mut https: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    let mut renewal_task: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(addr) = settings.https_listen.clone() {
        let cert_store = Arc::new(CertStore::new(PathBuf::from(&settings.cert_dir)));
        let now = renewal::now_secs();
        let shared = SharedCerts::new(CertResolver::from_certs(&cert_store.load_all(now))?);

        let https_listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("bind https {addr}"))?;
        https = Some(tokio::spawn(serve_https(
            https_listener,
            shared.clone(),
            routes.clone(),
        )));

        let issuer: Arc<dyn CertIssuer> = Arc::new(StoreIssuer {
            store: store.clone(),
            account_path: PathBuf::from(env_or(
                "HOSTER_ACME_ACCOUNT_FILE",
                "/var/lib/hoster/acme-account.json",
            )),
            // Staging by default: its certificates are not browser-trusted,
            // which makes a first run visibly prove the flow before
            // production's rate limits are at stake.
            production: env_flag("HOSTER_ACME_PRODUCTION"),
        });

        // Recomputed on every pass, so a project configured after startup gets
        // its certificate without a restart.
        let wanted_store = store.clone();
        let default_template = settings.hostname_template.clone();
        let wanted = move || {
            let mut out: Vec<String> = std::iter::once(default_template.clone())
                .chain(wanted_store.project_hostname_templates())
                .filter_map(|t| wildcard_base(&t))
                .collect();
            if let Some(h) = wanted_store.masked_acme().and_then(|a| a.control_hostname) {
                out.push(h);
            }
            out.sort();
            out.dedup();
            out
        };

        renewal_task = Some(tokio::spawn(renewal::run_loop(
            issuer, cert_store, shared, wanted,
        )));
    }

    let proxy = tokio::spawn(serve(proxy_listener, routes));
    let api = tokio::spawn(hoster::api::serve_api(api_listener, engine, settings));

    // `https` and `renewal_task` are `Option`s; a never-resolving future keeps
    // the `select!` arms well-formed when TLS is off.
    let https = async {
        match https {
            Some(h) => h.await.context("https task panicked")?,
            None => std::future::pending().await,
        }
    };
    let renewal_task = async {
        match renewal_task {
            Some(h) => h.await.context("renewal task panicked"),
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        r = proxy => r.context("proxy task panicked")?,
        r = api => r.context("api task panicked")?,
        r = https => r,
        r = renewal_task => r,
    }
}
