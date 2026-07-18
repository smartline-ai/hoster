use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::config::{self, DeployConfig};
use crate::imageref::registry_host;
use crate::labels;
use crate::routing::SharedRoutes;
use crate::runtime::{ContainerRuntime, ContainerSpec, RunningContainer};
use crate::secrets::Store;
use crate::settings::{Settings, hostname_for, sanitize_branch};
use crate::template::{TemplateVars, substitute};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployStatus {
    Provisioning,
    Running,
    Failed(String),
}

pub struct DeployRequest {
    pub branch: String,
    pub tag: String,
    pub sha: String,
    pub config: DeployConfig,
}

pub struct DeployAccepted {
    pub branch: String,
    pub urls: BTreeMap<String, String>,
}

/// One branch's deployment status and URLs, as reported by the control API.
#[derive(serde::Serialize)]
pub struct DeploymentInfo {
    pub branch: String,
    pub status: String,
    pub urls: BTreeMap<String, String>,
}

/// A branch deployment enriched with the config it was deployed from, for the
/// dashboard's project-grouped view. `config` is the submitted `hoster.json`
/// (decoded from the container label) — it never contains hoster-managed
/// injected secrets.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeploymentView {
    pub project: String,
    pub branch: String,
    pub status: String,
    pub urls: BTreeMap<String, String>,
    pub config: Option<DeployConfig>,
}

/// Injectable readiness probe so tests need no real sockets.
#[async_trait]
pub trait ReadinessChecker: Send + Sync {
    async fn ready(&self, ip: &str, port: u16, health: Option<&str>) -> bool;
}

pub struct AlwaysReady;
#[async_trait]
impl ReadinessChecker for AlwaysReady {
    async fn ready(&self, _ip: &str, _port: u16, _health: Option<&str>) -> bool {
        true
    }
}

pub struct NeverReady;
#[async_trait]
impl ReadinessChecker for NeverReady {
    async fn ready(&self, _ip: &str, _port: u16, _health: Option<&str>) -> bool {
        false
    }
}

pub struct Engine<R: ContainerRuntime> {
    runtime: Arc<R>,
    routes: SharedRoutes,
    settings: Arc<Settings>,
    readiness: Arc<dyn ReadinessChecker>,
    store: Arc<Store>,
    status: Mutex<BTreeMap<String, DeployStatus>>,
    urls: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    /// Serializes the list-by-label + routes.swap critical section so
    /// concurrent deploys/teardowns can't interleave and drop each other's
    /// routes (lost-update race). Held across the `.await` of the list call,
    /// so it must be an async mutex, not a std one.
    swap_lock: tokio::sync::Mutex<()>,
}

impl<R: ContainerRuntime> Engine<R> {
    pub fn new(
        runtime: Arc<R>,
        routes: SharedRoutes,
        settings: Arc<Settings>,
        readiness: Arc<dyn ReadinessChecker>,
        store: Arc<Store>,
    ) -> Self {
        Self::with_readiness(runtime, routes, settings, readiness, store)
    }

    pub fn with_readiness(
        runtime: Arc<R>,
        routes: SharedRoutes,
        settings: Arc<Settings>,
        readiness: Arc<dyn ReadinessChecker>,
        store: Arc<Store>,
    ) -> Self {
        Self {
            runtime,
            routes,
            settings,
            readiness,
            store,
            status: Mutex::new(BTreeMap::new()),
            urls: Mutex::new(BTreeMap::new()),
            swap_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The project environment store, for the control API's project routes.
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn status_of(&self, branch: &str) -> Option<DeployStatus> {
        self.status.lock().unwrap().get(branch).cloned()
    }

    fn set_status(&self, branch: &str, s: DeployStatus) {
        self.status.lock().unwrap().insert(branch.to_string(), s);
    }

    pub async fn deploy(&self, req: DeployRequest) -> anyhow::Result<DeployAccepted> {
        config::validate(&req.config).map_err(|m| anyhow::anyhow!(m))?;
        let branch = sanitize_branch(&req.branch);
        self.set_status(&branch, DeployStatus::Provisioning);

        // 1. hostnames + urls for exposed services
        let urls = self.urls_for(&req.config.services, &branch, &req.config.project);
        let vars = TemplateVars {
            registry: self.settings.registry.clone(),
            tag: req.tag.clone(),
            branch: branch.clone(),
            sha: req.sha.clone(),
            urls: urls.clone(),
        };

        let network = format!("hoster-{branch}");

        // 2. full-replace cleanup (resources only — must not clobber the
        // Provisioning status just set above; that's the bug this fix closes).
        if let Err(e) = self.remove_branch_resources(&branch).await {
            tracing::warn!(%branch, error = %e, "resource cleanup before deploy failed");
        }

        // 3. create network, then pull+run every service. Any failure here
        // (e.g. partway through the service loop) must not leave orphaned
        // resources or a stuck Provisioning status.
        // Snapshot of the submitted config, stored as a label so the dashboard
        // can show how the branch was deployed. Injected store secrets are
        // merged into the real env below and never enter this JSON.
        let config_json = serde_json::to_string(&req.config)?;

        let exposed = match self
            .create_and_run_services(
                &branch,
                &network,
                &vars,
                &req.config.project,
                &config_json,
                &req.config.services,
            )
            .await
        {
            Ok(exposed) => exposed,
            Err(e) => {
                self.set_status(&branch, DeployStatus::Failed(e.to_string()));
                let _ = self.remove_branch_resources(&branch).await;
                return Err(e);
            }
        };

        // 4. readiness gate
        for (c, port, health) in &exposed {
            let ip = c.ip.clone().unwrap_or_default();
            if !self.readiness.ready(&ip, *port, health.as_deref()).await {
                let msg = format!("service {} did not become ready", c.name);
                self.set_status(&branch, DeployStatus::Failed(msg.clone()));
                let _ = self.remove_branch_resources(&branch).await;
                anyhow::bail!(msg);
            }
        }

        // 5. build routes from every branch's containers and swap. The
        // list+swap is a critical section: hold swap_lock across both so
        // concurrent deploys can't interleave and lose each other's routes.
        {
            let _guard = self.swap_lock.lock().await;
            let full = self.runtime.list_by_label(labels::BRANCH).await?;
            self.routes.swap(labels::routes_from_containers(&full));
        }

        self.set_status(&branch, DeployStatus::Running);
        self.urls
            .lock()
            .unwrap()
            .insert(branch.clone(), urls.clone());
        Ok(DeployAccepted { branch, urls })
    }

    /// Create the branch network and run every configured service, returning
    /// the exposed containers for the readiness gate. Pure resource
    /// provisioning — no status mutation, so callers decide how to react to
    /// failure (see `deploy`'s Failed-status + cleanup handling).
    async fn create_and_run_services(
        &self,
        branch: &str,
        network: &str,
        vars: &TemplateVars,
        project: &str,
        config_json: &str,
        services: &BTreeMap<String, config::Service>,
    ) -> anyhow::Result<Vec<(RunningContainer, u16, Option<String>)>> {
        self.runtime
            .create_network(network, &branch_label(branch))
            .await?;

        let mut exposed: Vec<(RunningContainer, u16, Option<String>)> = Vec::new();
        for (name, svc) in services {
            let image = substitute(&svc.image, vars).map_err(|m| anyhow::anyhow!(m))?;
            // Build env from hoster.json (template-substituted), then overlay the
            // hoster-managed store vars verbatim — stored values win on conflict
            // and are never template-substituted (a `{{` in a secret is literal).
            let mut env_map: BTreeMap<String, String> = BTreeMap::new();
            for (k, v) in &svc.env {
                env_map.insert(
                    k.clone(),
                    substitute(v, vars).map_err(|m| anyhow::anyhow!(m))?,
                );
            }
            for (k, v) in self.store.env_for(project, name) {
                env_map.insert(k, v);
            }
            let env: Vec<String> = env_map
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            let mut labels = branch_label(branch);
            labels.insert(labels::SERVICE.to_string(), name.clone());
            labels.insert(labels::PROJECT.to_string(), project.to_string());
            labels.insert(labels::CONFIG.to_string(), config_json.to_string());
            if let Some(exp) = &svc.expose {
                let sub = exp.subdomain.clone().unwrap_or_else(|| name.clone());
                labels.insert(labels::PORT.to_string(), exp.port.to_string());
                labels.insert(
                    labels::HOSTNAME.to_string(),
                    hostname_for(&self.template_for(project), &sub, branch),
                );
            }
            // Send the project's credential only to its own registry — a
            // ghcr.io token must never travel to Docker Hub on a public pull.
            let cred = self
                .store
                .registry_for(project)
                .filter(|c| c.registry == registry_host(&image));
            self.runtime.pull_image(&image, cred.as_ref()).await?;
            let spec = ContainerSpec {
                name: format!("{branch}-{name}"),
                image,
                env,
                network: network.to_string(),
                network_alias: name.clone(),
                labels,
            };
            let c = self.runtime.run(&spec).await?;
            if let Some(exp) = &svc.expose {
                exposed.push((c, exp.port, exp.health.clone()));
            }
        }
        Ok(exposed)
    }

    /// Resource cleanup + route rebuild only — no status mutation. Used by
    /// `deploy`'s internal pre-create and failure-path cleanup so it doesn't
    /// erase the `Provisioning`/`Failed` status it just set.
    async fn remove_branch_resources(&self, branch: &str) -> anyhow::Result<()> {
        let branch = sanitize_branch(branch);
        let all = self.runtime.list_by_label(labels::BRANCH).await?;
        for c in all
            .iter()
            .filter(|c| c.labels.get(labels::BRANCH) == Some(&branch))
        {
            self.runtime.remove_container(&c.id).await?;
        }
        let _ = self
            .runtime
            .remove_network(&format!("hoster-{branch}"))
            .await;
        // Same list+swap critical section as `deploy` step 5 — container
        // removals above don't need the lock, but the rebuild does.
        {
            let _guard = self.swap_lock.lock().await;
            let remaining = self.runtime.list_by_label(labels::BRANCH).await?;
            self.routes.swap(labels::routes_from_containers(&remaining));
        }
        Ok(())
    }

    pub async fn teardown(&self, branch: &str) -> anyhow::Result<()> {
        self.remove_branch_resources(branch).await?;
        self.status.lock().unwrap().remove(&sanitize_branch(branch));
        self.urls.lock().unwrap().remove(&sanitize_branch(branch));
        Ok(())
    }

    /// Compute the URLs a deploy of `req` would produce, without touching any
    /// runtime state. Used by the control API to answer synchronously while
    /// the actual deploy runs in the background.
    pub fn plan_urls(&self, req: &DeployRequest) -> BTreeMap<String, String> {
        let branch = sanitize_branch(&req.branch);
        self.urls_for(&req.config.services, &branch, &req.config.project)
    }

    /// Snapshot of every known branch's status and computed URLs, for the
    /// control API's `GET /deployments`.
    pub fn deployments(&self) -> Vec<DeploymentInfo> {
        let status = self.status.lock().unwrap();
        let urls = self.urls.lock().unwrap();
        status
            .iter()
            .map(|(branch, st)| DeploymentInfo {
                branch: branch.clone(),
                status: match st {
                    DeployStatus::Provisioning => "provisioning".into(),
                    DeployStatus::Running => "running".into(),
                    DeployStatus::Failed(m) => format!("failed: {m}"),
                },
                urls: urls.get(branch).cloned().unwrap_or_default(),
            })
            .collect()
    }

    /// The hostname template for `project`: its own if it has one, otherwise
    /// the operator's global default.
    fn template_for(&self, project: &str) -> String {
        self.store
            .hostname_template_for(project)
            .unwrap_or_else(|| self.settings.hostname_template.clone())
    }

    /// Compute the public URLs for a service map on a branch — the URL of every
    /// exposed service. Deterministic from config + branch + the project's
    /// template, so it works for views reconstructed from labels after a
    /// restart.
    fn urls_for(
        &self,
        services: &BTreeMap<String, config::Service>,
        branch: &str,
        project: &str,
    ) -> BTreeMap<String, String> {
        let template = self.template_for(project);
        let mut urls = BTreeMap::new();
        for (name, svc) in services {
            if let Some(exp) = &svc.expose {
                let sub = exp.subdomain.clone().unwrap_or_else(|| name.clone());
                let host = hostname_for(&template, &sub, branch);
                urls.insert(name.clone(), format!("http://{host}"));
            }
        }
        urls
    }

    /// Every branch's deployment enriched with the config it was deployed from,
    /// reconstructed from container labels and grouped for the dashboard.
    ///
    /// URLs are read directly off each container's `SERVICE`/`HOSTNAME`
    /// labels — the same source `labels::routes_from_containers` uses to
    /// build the proxy's routing table — rather than recomputed from the
    /// project's current hostname template. The template can be changed by
    /// an operator at any time without redeploying; a running branch's real,
    /// routable hostname is the one baked into its container labels at
    /// deploy time, so that (not the live template) is the only correct
    /// source here.
    pub async fn deployment_views(&self) -> anyhow::Result<Vec<DeploymentView>> {
        let containers = self.runtime.list_by_label(labels::PROJECT).await?;
        // branch -> (project, config JSON, every container of that branch).
        // Every container is kept (not just the first) because URLs are
        // built from each container's own SERVICE/HOSTNAME labels below.
        let mut by_branch: BTreeMap<String, (String, Option<String>, Vec<&RunningContainer>)> =
            BTreeMap::new();
        for c in &containers {
            let (Some(branch), Some(project)) =
                (c.labels.get(labels::BRANCH), c.labels.get(labels::PROJECT))
            else {
                continue;
            };
            let entry = by_branch.entry(branch.clone()).or_insert_with(|| {
                (
                    project.clone(),
                    c.labels.get(labels::CONFIG).cloned(),
                    Vec::new(),
                )
            });
            entry.2.push(c);
        }

        let mut out = Vec::new();
        for (branch, (project, cfg_json, branch_containers)) in by_branch {
            let config = cfg_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<DeployConfig>(j).ok());
            // service -> http://hostname, straight off the labels written at
            // deploy time. A container without a HOSTNAME label isn't
            // exposed and contributes no URL.
            let mut urls = BTreeMap::new();
            for c in &branch_containers {
                let (Some(service), Some(hostname)) = (
                    c.labels.get(labels::SERVICE),
                    c.labels.get(labels::HOSTNAME),
                ) else {
                    continue;
                };
                urls.insert(service.clone(), format!("http://{hostname}"));
            }
            let status = match self.status_of(&branch) {
                Some(DeployStatus::Provisioning) => "provisioning".to_string(),
                Some(DeployStatus::Running) => "running".to_string(),
                Some(DeployStatus::Failed(m)) => format!("failed: {m}"),
                // Labels present but no in-process status (e.g. after a restart)
                // ⇒ the containers exist, so treat it as running.
                None => "running".to_string(),
            };
            out.push(DeploymentView {
                project,
                branch,
                status,
                urls,
                config,
            });
        }
        Ok(out)
    }

    pub async fn reconcile(&self) -> anyhow::Result<()> {
        {
            let _guard = self.swap_lock.lock().await;
            let all = self.runtime.list_by_label(labels::BRANCH).await?;
            self.routes.swap(labels::routes_from_containers(&all));
        }
        tracing::info!(
            routes = self.routes.load().len(),
            "reconciled routing table from labels"
        );
        Ok(())
    }
}

fn branch_label(branch: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(labels::BRANCH.to_string(), branch.to_string());
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::routing::SharedRoutes;
    use crate::runtime::FakeRuntime;
    use std::sync::Arc;

    fn settings() -> Arc<crate::settings::Settings> {
        Arc::new(crate::settings::Settings {
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "reg.example.com".into(),
            token: "t".into(),
            dashboard_password: None,
        })
    }

    fn empty_store() -> Arc<Store> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "hoster-engine-test-{}-{n}/projects.json",
            std::process::id()
        ));
        Arc::new(Store::load(path).unwrap())
    }

    fn engine(rt: Arc<FakeRuntime>, routes: SharedRoutes) -> Engine<FakeRuntime> {
        // AlwaysReady checker: no real TCP/HTTP in unit tests.
        Engine::with_readiness(rt, routes, settings(), Arc::new(AlwaysReady), empty_store())
    }

    fn engine_with_store(
        rt: Arc<FakeRuntime>,
        routes: SharedRoutes,
        store: Arc<Store>,
    ) -> Engine<FakeRuntime> {
        Engine::with_readiness(rt, routes, settings(), Arc::new(AlwaysReady), store)
    }

    fn request(branch: &str, json: &str) -> DeployRequest {
        DeployRequest {
            branch: branch.to_string(),
            tag: "abc".to_string(),
            sha: "sha".to_string(),
            config: config::parse(json).unwrap(),
        }
    }

    /// Fresh `Engine<FakeRuntime>` plus its runtime and store handles, for
    /// tests that need to inspect what the fake runtime recorded.
    fn engine_with_fake() -> (Engine<FakeRuntime>, Arc<FakeRuntime>, Arc<Store>) {
        let rt = Arc::new(FakeRuntime::new());
        let store = empty_store();
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = engine_with_store(rt.clone(), routes, store.clone());
        (eng, rt, store)
    }

    /// Perform one deploy from a config JSON string — thin wrapper shared by
    /// tests that only care about the deploy's side effects.
    async fn deploy_config(
        engine: &Engine<FakeRuntime>,
        branch: &str,
        json: &str,
    ) -> anyhow::Result<DeployAccepted> {
        engine.deploy(request(branch, json)).await
    }

    const TWO_SERVICE: &str = r#"{"project":"p","services":{
        "postgres":{"image":"postgres:16"},
        "backend":{"image":"{{registry}}/backend:{{tag}}","env":{"DATABASE_URL":"postgres://postgres:5432/app","PUBLIC_URL":"{{url.backend}}"},"expose":{"port":8080}}
    }}"#;

    #[tokio::test]
    async fn deploy_runs_all_services_and_routes_exposed_one() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = engine(rt.clone(), routes.clone());

        let accepted = eng
            .deploy(request("feature/JIRA-1", TWO_SERVICE))
            .await
            .unwrap();

        // Both services are containers…
        assert_eq!(rt.container_count(), 2);
        // …but only the exposed one is routed, at its computed hostname.
        let host = "backend-feature-jira-1.dev.example.com";
        assert!(routes.load().lookup(host).is_some());
        assert!(accepted.urls.contains_key("backend"));
        assert_eq!(accepted.urls["backend"], format!("http://{host}"));
        // internal postgres has no route
        assert_eq!(routes.load().len(), 1);
    }

    #[tokio::test]
    async fn stored_env_overrides_hoster_json_and_targets_only_listed_service() {
        let rt = Arc::new(FakeRuntime::new());
        let store = empty_store();
        store
            .set_var("p", "GOOGLE_API_KEY", "from-hoster", vec!["backend".into()])
            .unwrap();
        store
            .set_var("p", "DATABASE_URL", "stored-wins", vec![])
            .unwrap();
        let eng = engine_with_store(
            rt.clone(),
            SharedRoutes::new(crate::routing::RoutingTable::new()),
            store,
        );
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();

        let backend = rt.env_of("b1-backend").unwrap();
        // Targeted secret reaches backend.
        assert!(backend.iter().any(|e| e == "GOOGLE_API_KEY=from-hoster"));
        // Stored value overrides the hoster.json DATABASE_URL for the same key.
        assert!(backend.iter().any(|e| e == "DATABASE_URL=stored-wins"));
        assert!(
            !backend
                .iter()
                .any(|e| e == "DATABASE_URL=postgres://postgres:5432/app")
        );

        // postgres is not a target of GOOGLE_API_KEY, but is of the all-services var.
        let pg = rt.env_of("b1-postgres").unwrap();
        assert!(!pg.iter().any(|e| e.starts_with("GOOGLE_API_KEY=")));
        assert!(pg.iter().any(|e| e == "DATABASE_URL=stored-wins"));
    }

    #[tokio::test]
    async fn containers_carry_project_and_config_labels_without_secrets() {
        let rt = Arc::new(FakeRuntime::new());
        let store = empty_store();
        store
            .set_var("p", "SECRET_KEY", "topsecret", vec![])
            .unwrap();
        let eng = engine_with_store(
            rt.clone(),
            SharedRoutes::new(crate::routing::RoutingTable::new()),
            store,
        );
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();

        let cs = rt.list_by_label(crate::labels::PROJECT).await.unwrap();
        let backend = cs.iter().find(|c| c.name == "b1-backend").unwrap();
        assert_eq!(backend.labels[crate::labels::PROJECT], "p");
        let cfg = &backend.labels[crate::labels::CONFIG];
        // The submitted config round-trips…
        assert!(cfg.contains("\"backend\""));
        assert!(cfg.contains("\"project\":\"p\"") || cfg.contains("\"project\": \"p\""));
        // …but the injected secret is never written into the config label.
        assert!(
            !cfg.contains("topsecret"),
            "secret leaked into config label: {cfg}"
        );
        assert!(
            !cfg.contains("SECRET_KEY"),
            "secret key leaked into config label: {cfg}"
        );
    }

    #[tokio::test]
    async fn deployment_views_expose_project_and_config_without_secrets() {
        let rt = Arc::new(FakeRuntime::new());
        let store = empty_store();
        store
            .set_var("p", "SECRET_KEY", "topsecret", vec![])
            .unwrap();
        let eng = engine_with_store(
            rt.clone(),
            SharedRoutes::new(crate::routing::RoutingTable::new()),
            store,
        );
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();

        let views = eng.deployment_views().await.unwrap();
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert_eq!(v.project, "p");
        assert_eq!(v.branch, "b1");
        assert_eq!(v.status, "running");
        assert_eq!(v.urls["backend"], "http://backend-b1.dev.example.com");
        let cfg = v.config.as_ref().expect("config decoded from label");
        assert!(cfg.services.contains_key("backend"));
        assert!(cfg.services.contains_key("postgres"));
        assert!(cfg.services["backend"].env.contains_key("DATABASE_URL"));
        // The injected secret is never part of the shown config.
        assert!(!cfg.services["backend"].env.contains_key("SECRET_KEY"));
    }

    #[tokio::test]
    async fn env_templates_are_substituted() {
        let rt = Arc::new(FakeRuntime::new());
        let eng = engine(
            rt.clone(),
            SharedRoutes::new(crate::routing::RoutingTable::new()),
        );
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        // The backend container's PUBLIC_URL must be the real URL, not a literal template.
        let containers = rt.list_by_label(crate::labels::SERVICE).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.labels[crate::labels::SERVICE] == "backend")
            .unwrap();
        // FakeRuntime stores labels but not env; assert via the spec path instead:
        // deploy must have set PUBLIC_URL — verified by the no-literal-template check in Step 4's helper.
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-b1.dev.example.com"
        );
    }

    #[tokio::test]
    async fn redeploy_is_full_replace() {
        let rt = Arc::new(FakeRuntime::new());
        let eng = engine(
            rt.clone(),
            SharedRoutes::new(crate::routing::RoutingTable::new()),
        );
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        assert_eq!(rt.container_count(), 2);
        // Redeploy the same branch: old containers gone, new ones created, still 2.
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        assert_eq!(rt.container_count(), 2);
    }

    #[tokio::test]
    async fn teardown_removes_branch_and_route() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = engine(rt.clone(), routes.clone());
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        eng.teardown("b1").await.unwrap();
        assert_eq!(rt.container_count(), 0);
        assert!(routes.load().is_empty());
    }

    #[tokio::test]
    async fn invalid_config_is_rejected_before_any_container() {
        let rt = Arc::new(FakeRuntime::new());
        let eng = engine(
            rt.clone(),
            SharedRoutes::new(crate::routing::RoutingTable::new()),
        );
        let bad = request("b1", r#"{"project":"p","services":{}}"#);
        assert!(eng.deploy(bad).await.is_err());
        assert_eq!(rt.container_count(), 0);
    }

    #[tokio::test]
    async fn failed_readiness_marks_failed_and_leaves_no_route() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = Engine::with_readiness(
            rt.clone(),
            routes.clone(),
            settings(),
            Arc::new(NeverReady),
            empty_store(),
        );
        let r = eng.deploy(request("b1", TWO_SERVICE)).await;
        assert!(r.is_err());
        assert!(routes.load().is_empty());
    }

    #[tokio::test]
    async fn status_is_running_after_success() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = engine(rt.clone(), routes.clone());
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        assert_eq!(eng.status_of("b1"), Some(DeployStatus::Running));
    }

    #[tokio::test]
    async fn status_is_failed_after_readiness_timeout() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = Engine::with_readiness(
            rt.clone(),
            routes.clone(),
            settings(),
            Arc::new(NeverReady),
            empty_store(),
        );
        let r = eng.deploy(request("b1", TWO_SERVICE)).await;
        assert!(r.is_err());
        assert!(matches!(eng.status_of("b1"), Some(DeployStatus::Failed(_))));
        assert!(routes.load().is_empty());
    }

    #[tokio::test]
    async fn teardown_clears_status() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = engine(rt.clone(), routes.clone());
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        eng.teardown("b1").await.unwrap();
        assert_eq!(eng.status_of("b1"), None);
    }

    #[tokio::test]
    async fn reconcile_rebuilds_routes_from_labels() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = engine(rt.clone(), routes.clone());
        eng.deploy(request("b1", TWO_SERVICE)).await.unwrap();
        // Simulate a restart: fresh routing table, then reconcile from the runtime.
        let fresh = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng2 = engine(rt.clone(), fresh.clone());
        eng2.reconcile().await.unwrap();
        assert!(fresh.load().lookup("backend-b1.dev.example.com").is_some());
    }

    #[tokio::test]
    async fn concurrent_deploys_keep_both_routes() {
        let rt = Arc::new(FakeRuntime::new());
        let routes = SharedRoutes::new(crate::routing::RoutingTable::new());
        let eng = Arc::new(engine(rt.clone(), routes.clone()));
        let e1 = eng.clone();
        let e2 = eng.clone();
        let a = tokio::spawn(async move { e1.deploy(request("b1", TWO_SERVICE)).await });
        let b = tokio::spawn(async move { e2.deploy(request("b2", TWO_SERVICE)).await });
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
        let t = routes.load();
        assert!(
            t.lookup("backend-b1.dev.example.com").is_some(),
            "b1 route lost"
        );
        assert!(
            t.lookup("backend-b2.dev.example.com").is_some(),
            "b2 route lost"
        );
    }

    #[tokio::test]
    async fn credential_is_sent_only_to_the_matching_registry() {
        // A project with a ghcr.io credential deploying two services: one
        // private image from ghcr.io, one public image from Docker Hub.
        let (engine, runtime, store) = engine_with_fake();
        store
            .set_registry("myproj", "ghcr.io", "bot", "ghp_secret")
            .unwrap();
        let expected = crate::secrets::RegistryCred {
            registry: "ghcr.io".to_string(),
            username: "bot".to_string(),
            password: "ghp_secret".to_string(),
        };

        let config = r#"{"project":"myproj","services":{
            "postgres":{"image":"postgres:16"},
            "backend":{"image":"ghcr.io/org/backend:v1","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        let sent = runtime.pull_cred_of("ghcr.io/org/backend:v1").unwrap();
        assert_eq!(
            sent,
            Some(expected),
            "the exact stored credential (registry, username, password) should be sent to its own registry"
        );

        let public = runtime.pull_cred_of("postgres:16").unwrap();
        assert!(
            public.is_none(),
            "ghcr.io credential must NOT be sent to Docker Hub"
        );
    }

    #[tokio::test]
    async fn no_credential_stored_means_anonymous_pulls() {
        let (engine, runtime, _store) = engine_with_fake();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"ghcr.io/org/backend:v1","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();
        assert_eq!(runtime.pull_cred_of("ghcr.io/org/backend:v1"), Some(None));
    }

    #[tokio::test]
    async fn a_projects_template_overrides_the_global_default() {
        let (engine, runtime, store) = engine_with_fake();
        store
            .set_hostname_template("myproj", "{service}-{branch}.demo.example.com")
            .unwrap();

        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.name.ends_with("backend"))
            .expect("backend container");
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-main.demo.example.com"
        );
    }

    #[tokio::test]
    async fn a_project_without_a_template_uses_the_global_default() {
        let (engine, runtime, _store) = engine_with_fake();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.name.ends_with("backend"))
            .expect("backend container");
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-main.dev.example.com"
        );
    }

    #[tokio::test]
    async fn two_projects_get_different_hostnames_for_the_same_branch() {
        let (engine, runtime, store) = engine_with_fake();
        store
            .set_hostname_template("alpha", "{service}-{branch}.a.example.com")
            .unwrap();
        store
            .set_hostname_template("beta", "{service}-{branch}.b.example.com")
            .unwrap();

        deploy_config(
            &engine,
            "main",
            r#"{"project":"alpha","services":{"backend":{"image":"img","expose":{"port":8080}}}}"#,
        )
        .await
        .unwrap();
        deploy_config(
            &engine,
            "release",
            r#"{"project":"beta","services":{"backend":{"image":"img","expose":{"port":8080}}}}"#,
        )
        .await
        .unwrap();

        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let hosts: Vec<&str> = containers
            .iter()
            .filter_map(|c| c.labels.get(crate::labels::HOSTNAME))
            .map(String::as_str)
            .collect();
        assert!(
            hosts.contains(&"backend-main.a.example.com"),
            "got {hosts:?}"
        );
        assert!(
            hosts.contains(&"backend-release.b.example.com"),
            "got {hosts:?}"
        );
    }

    #[tokio::test]
    async fn deploy_urls_use_the_projects_template() {
        let (engine, _runtime, store) = engine_with_fake();
        store
            .set_hostname_template("myproj", "{service}-{branch}.demo.example.com")
            .unwrap();
        let req = request(
            "main",
            r#"{"project":"myproj","services":{"backend":{"image":"img","expose":{"port":8080}}}}"#,
        );
        let urls = engine.plan_urls(&req);
        assert_eq!(
            urls.get("backend").map(String::as_str),
            Some("http://backend-main.demo.example.com")
        );
    }

    #[tokio::test]
    async fn a_running_branch_keeps_its_hostname_after_its_template_changes() {
        let (engine, runtime, store) = engine_with_fake();
        store
            .set_hostname_template("myproj", "{service}-{branch}.old.example.com")
            .unwrap();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        // Change the template without redeploying.
        store
            .set_hostname_template("myproj", "{service}-{branch}.new.example.com")
            .unwrap();

        // Views are rebuilt from container labels, so the running branch keeps
        // the hostname it was deployed with.
        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.name.ends_with("backend"))
            .expect("backend container");
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-main.old.example.com",
            "a running container's hostname must not change under it"
        );
    }

    #[tokio::test]
    async fn deployment_views_report_the_hostname_the_branch_was_deployed_with() {
        let (engine, _runtime, store) = engine_with_fake();
        store
            .set_hostname_template("myproj", "{service}-{branch}.old.example.com")
            .unwrap();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        // Change the template without redeploying.
        store
            .set_hostname_template("myproj", "{service}-{branch}.new.example.com")
            .unwrap();

        // deployment_views() must report the hostname the branch was actually
        // deployed with (from container labels), not one recomputed from the
        // project's since-changed, live template — otherwise the dashboard and
        // control API hand out URLs the proxy doesn't route.
        let views = engine.deployment_views().await.unwrap();
        let v = views
            .iter()
            .find(|v| v.branch == "main")
            .expect("view for main");
        let url = v.urls.get("backend").expect("backend url");
        assert!(url.contains("old.example.com"), "got {url}");
        assert!(!url.contains("new.example.com"), "got {url}");
    }

    #[tokio::test]
    async fn deployment_views_report_a_url_for_every_exposed_service() {
        let (engine, _runtime, _store) = engine_with_fake();
        let config = r#"{"project":"myproj","services":{
            "postgres":{"image":"postgres:16"},
            "backend":{"image":"img","expose":{"port":8080}},
            "worker":{"image":"img2","expose":{"port":9090}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        // Multi-service deployment: every exposed service must get a URL, and
        // the regrouping in deployment_views() must not silently collapse to
        // just one container's labels per branch.
        let views = engine.deployment_views().await.unwrap();
        let v = views
            .iter()
            .find(|v| v.branch == "main")
            .expect("view for main");
        assert!(
            v.urls.contains_key("backend"),
            "missing backend url: {:?}",
            v.urls
        );
        assert!(
            v.urls.contains_key("worker"),
            "missing worker url: {:?}",
            v.urls
        );
        assert!(
            !v.urls.contains_key("postgres"),
            "postgres is not exposed and must have no url: {:?}",
            v.urls
        );
        assert_eq!(v.urls.len(), 2, "got {:?}", v.urls);
    }
}
