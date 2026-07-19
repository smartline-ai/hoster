use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use hoster::acme::{CertIssuer, IssuedCert, Issuer};
use hoster::certs::CertStore;
use hoster::docker::DockerRuntime;
use hoster::engine::Engine;
use hoster::proxy::serve;
use hoster::readiness::NetworkReadiness;
use hoster::renewal;
use hoster::routing::{RoutingTable, SharedRoutes};
use hoster::secrets::Store;
use hoster::session::Sessions;
use hoster::settings::{ProxyMode, Settings};
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
    settings: Arc<Settings>,
    account_path: PathBuf,
    production: bool,
}

#[async_trait::async_trait]
impl CertIssuer for StoreIssuer {
    async fn issue(&self, domain: &str) -> anyhow::Result<IssuedCert> {
        // Credentials are read from the store and handed straight to the
        // provider (built in `build_provider` below); they are never logged or
        // returned on this path.
        let cfg = self
            .store
            .acme_config()
            .ok_or_else(|| anyhow::anyhow!("ACME is not configured (no account email)"))?;
        // Resolved per domain rather than once globally: a project with its
        // own DNS provider override wins for the base it owns, and only
        // falls back to the global default (or errors) otherwise — see
        // `Store::dns_provider_for`.
        let provider_cfg = self
            .store
            .dns_provider_for(domain, &self.settings.hostname_template)
            .ok_or_else(|| anyhow::anyhow!("no DNS provider configured for {domain}"))?;
        if provider_cfg.kind == "manual" {
            anyhow::bail!(
                "{domain} uses the manual DNS provider; hoster cannot answer its DNS-01 challenge"
            );
        }
        let client_ip = self.settings.public_ip.clone().unwrap_or_default();
        let dns = hoster::dns::build_provider(&provider_cfg, &client_ip)?;
        let issuer = Issuer::new(self.account_path.clone(), cfg.email, dns);
        let issuer = if self.production {
            issuer.use_production()
        } else {
            issuer
        };
        issuer.issue_cert(domain).await
    }
}

/// Terminate TLS and dispatch each request by `Host`: hoster's own control
/// hostname goes to the API/dashboard handler, everything else to the same
/// proxy service the plain listener uses.
///
/// Serving the control hostname here is what makes the certificate hoster
/// issues for it worth issuing — it is in the renewal loop's `wanted` set, and
/// a certificate nothing ever presents is pure waste. Auth is unchanged:
/// requests go through the very same `handle_api`, so the API's bearer gate
/// and the dashboard's cookie session behave exactly as they do on
/// `api_listen`. The control hostname is read from the store per request, so
/// configuring it through the dashboard takes effect without a restart.
async fn serve_https(
    listener: TcpListener,
    certs: SharedCerts,
    routes: SharedRoutes,
    engine: Arc<Engine<DockerRuntime>>,
    settings: Arc<Settings>,
    sessions: Arc<Sessions>,
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
        let engine = engine.clone();
        let settings = settings.clone();
        let sessions = sessions.clone();
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
            let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let routes = routes.clone();
                let client = client.clone();
                let engine = engine.clone();
                let settings = settings.clone();
                let sessions = sessions.clone();
                async move {
                    let control = engine
                        .store()
                        .masked_acme()
                        .and_then(|a| a.control_hostname);
                    let host = req
                        .headers()
                        .get(hyper::header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    if hoster::api::is_control_host(host.as_deref(), control.as_deref()) {
                        // `api::ApiBody` and `proxy::ProxyBody` are both
                        // `BoxBody<Bytes, BoxError>`, so the control-host
                        // response needs no body adaptation.
                        return hoster::api::handle_api(req, engine, settings, sessions).await;
                    }
                    hoster::proxy::handle(req, routes, client, hoster::proxy::Scheme::Https).await
                }
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
        public_ip: std::env::var("HOSTER_PUBLIC_IP")
            .ok()
            .filter(|v| !v.trim().is_empty()),
        proxy_mode: ProxyMode::parse(&env_or("HOSTER_PROXY_MODE", "standalone"))?,
        nginx_conf_path: env_or("HOSTER_NGINX_CONF", "/etc/nginx/conf.d/hoster.conf"),
        nginx_reload_cmd: env_or("HOSTER_NGINX_RELOAD_CMD", "systemctl reload nginx"),
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
    // The trigger is only attached when TLS is on: with no renewal loop
    // running there is nothing to trigger, and the API reports that instead
    // of accepting a request that would do nothing.
    let renewal_trigger = renewal::RenewalTrigger::new();
    let engine = Engine::new(
        runtime,
        routes.clone(),
        settings.clone(),
        Arc::new(NetworkReadiness::default()),
        store.clone(),
    );
    let engine = Arc::new(if settings.https_listen.is_some() {
        engine.with_renewal_trigger(renewal_trigger.clone())
    } else {
        engine
    });

    // One session table shared by both listeners, so a dashboard login on the
    // control hostname is the same session as one on `api_listen`.
    let sessions = Arc::new(Sessions::new());

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
            engine.clone(),
            settings.clone(),
            sessions.clone(),
        )));

        let issuer: Arc<dyn CertIssuer> = Arc::new(StoreIssuer {
            store: store.clone(),
            settings: settings.clone(),
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
        // its certificate without a restart. The dashboard's certificate table
        // calls the same `wanted_domains`, so the two can never drift.
        let wanted_store = store.clone();
        let default_template = settings.hostname_template.clone();
        let wanted = move || renewal::wanted_domains(&wanted_store, &default_template);

        renewal_task = Some(tokio::spawn(renewal::run_loop(
            issuer,
            cert_store,
            shared,
            wanted,
            renewal_trigger.clone(),
        )));
    }

    let proxy = tokio::spawn(serve(proxy_listener, routes));
    let api = tokio::spawn(hoster::api::serve_api_with_sessions(
        api_listener,
        engine,
        settings,
        sessions,
    ));

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

#[cfg(test)]
mod store_issuer_tests {
    use super::*;
    use hoster::secrets::DnsProviderConfig;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A unique, non-existent store path per test, so parallel test runs
    /// never share (or race on) a projects.json file.
    fn temp_store_path() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "hoster-main-store-issuer-test-{}-{n}.json",
            std::process::id()
        ))
    }

    fn test_settings() -> Arc<Settings> {
        Arc::new(Settings {
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: String::new(),
            token: "t".into(),
            dashboard_password: None,
            https_listen: None,
            cert_dir: "/tmp".into(),
            public_ip: None,
            proxy_mode: ProxyMode::Standalone,
            nginx_conf_path: "/etc/nginx/conf.d/hoster.conf".into(),
            nginx_reload_cmd: "systemctl reload nginx".into(),
        })
    }

    fn store_issuer(store: Store) -> StoreIssuer {
        StoreIssuer {
            store: Arc::new(store),
            settings: test_settings(),
            account_path: PathBuf::from("/tmp/hoster-main-test-account.json"),
            production: false,
        }
    }

    /// `Result::unwrap_err` requires `T: Debug`, which `IssuedCert`
    /// deliberately does not implement (it carries a private key). Extract
    /// the error by hand instead of deriving `Debug` onto certificate
    /// material just to satisfy a test helper.
    fn expect_err(result: anyhow::Result<IssuedCert>) -> anyhow::Error {
        match result {
            Ok(_) => panic!("expected an error, got a successfully issued certificate"),
            Err(e) => e,
        }
    }

    // Both cases below resolve (or fail to resolve) a DNS provider before
    // `Issuer::issue_cert` is ever reached, so neither one touches the
    // network — the manual-bail and no-provider-configured paths return
    // straight out of `StoreIssuer::issue` per-domain resolution.

    #[tokio::test]
    async fn issue_bails_clearly_for_a_manually_managed_domain() {
        let store = Store::load(temp_store_path()).unwrap();
        store.set_acme_config("ops@example.com", None).unwrap();
        store
            .set_dns_provider(DnsProviderConfig {
                kind: "manual".into(),
                token: None,
                api_user: None,
                api_key: None,
                username: None,
            })
            .unwrap();
        let issuer = store_issuer(store);

        let err = expect_err(issuer.issue("*.dev.example.com").await);
        assert!(
            err.to_string().contains("manual"),
            "error should name the manual DNS provider: {err}"
        );
    }

    #[tokio::test]
    async fn issue_errors_clearly_when_no_dns_provider_is_configured() {
        let store = Store::load(temp_store_path()).unwrap();
        store.set_acme_config("ops@example.com", None).unwrap();
        let issuer = store_issuer(store);

        let err = expect_err(issuer.issue("*.dev.example.com").await);
        assert!(
            err.to_string().contains("no DNS provider"),
            "error should say no provider is configured: {err}"
        );
    }

    #[tokio::test]
    async fn issue_resolves_the_projects_own_provider_over_the_global_default() {
        // Exercises the per-domain resolution end to end up to (but not
        // including) the network call: the project "alpha" has its own
        // manual override, so its base must bail with the manual error even
        // though the global default below is a non-manual kind.
        let store = Store::load(temp_store_path()).unwrap();
        store.set_acme_config("ops@example.com", None).unwrap();
        store
            .set_dns_provider(DnsProviderConfig {
                kind: "hetzner".into(),
                token: Some("global-token".into()),
                api_user: None,
                api_key: None,
                username: None,
            })
            .unwrap();
        store
            .set_hostname_template("alpha", "{service}-{branch}.alpha.example.com")
            .unwrap();
        store
            .set_project_dns_provider(
                "alpha",
                DnsProviderConfig {
                    kind: "manual".into(),
                    token: None,
                    api_user: None,
                    api_key: None,
                    username: None,
                },
            )
            .unwrap();
        let issuer = store_issuer(store);

        let err = expect_err(issuer.issue("*.alpha.example.com").await);
        assert!(
            err.to_string().contains("manual"),
            "alpha's own manual override should win over the global hetzner default: {err}"
        );
    }
}
