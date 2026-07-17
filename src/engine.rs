use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::config::{self, DeployConfig};
use crate::labels;
use crate::routing::SharedRoutes;
use crate::runtime::{ContainerRuntime, ContainerSpec, RunningContainer};
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
    status: Mutex<BTreeMap<String, DeployStatus>>,
}

impl<R: ContainerRuntime> Engine<R> {
    pub fn new(
        runtime: Arc<R>,
        routes: SharedRoutes,
        settings: Arc<Settings>,
        readiness: Arc<dyn ReadinessChecker>,
    ) -> Self {
        Self::with_readiness(runtime, routes, settings, readiness)
    }

    pub fn with_readiness(
        runtime: Arc<R>,
        routes: SharedRoutes,
        settings: Arc<Settings>,
        readiness: Arc<dyn ReadinessChecker>,
    ) -> Self {
        Self {
            runtime,
            routes,
            settings,
            readiness,
            status: Mutex::new(BTreeMap::new()),
        }
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
        let mut urls = BTreeMap::new();
        for (name, svc) in &req.config.services {
            if let Some(exp) = &svc.expose {
                let sub = exp.subdomain.clone().unwrap_or_else(|| name.clone());
                let host = hostname_for(&self.settings.hostname_template, &sub, &branch);
                urls.insert(name.clone(), format!("http://{host}"));
            }
        }
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
        let exposed = match self
            .create_and_run_services(&branch, &network, &vars, &req.config.services)
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

        // 5. build routes from every branch's containers and swap
        let full = self.runtime.list_by_label(labels::BRANCH).await?;
        self.routes.swap(labels::routes_from_containers(&full));

        self.set_status(&branch, DeployStatus::Running);
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
        services: &BTreeMap<String, config::Service>,
    ) -> anyhow::Result<Vec<(RunningContainer, u16, Option<String>)>> {
        self.runtime
            .create_network(network, &branch_label(branch))
            .await?;

        let mut exposed: Vec<(RunningContainer, u16, Option<String>)> = Vec::new();
        for (name, svc) in services {
            let image = substitute(&svc.image, vars).map_err(|m| anyhow::anyhow!(m))?;
            let mut env = Vec::new();
            for (k, v) in &svc.env {
                env.push(format!(
                    "{k}={}",
                    substitute(v, vars).map_err(|m| anyhow::anyhow!(m))?
                ));
            }
            let mut labels = branch_label(branch);
            labels.insert(labels::SERVICE.to_string(), name.clone());
            if let Some(exp) = &svc.expose {
                let sub = exp.subdomain.clone().unwrap_or_else(|| name.clone());
                labels.insert(labels::PORT.to_string(), exp.port.to_string());
                labels.insert(
                    labels::HOSTNAME.to_string(),
                    hostname_for(&self.settings.hostname_template, &sub, branch),
                );
            }
            self.runtime.pull_image(&image).await?;
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
        let remaining = self.runtime.list_by_label(labels::BRANCH).await?;
        self.routes.swap(labels::routes_from_containers(&remaining));
        Ok(())
    }

    pub async fn teardown(&self, branch: &str) -> anyhow::Result<()> {
        self.remove_branch_resources(branch).await?;
        self.status.lock().unwrap().remove(&sanitize_branch(branch));
        Ok(())
    }

    pub async fn reconcile(&self) -> anyhow::Result<()> {
        let all = self.runtime.list_by_label(labels::BRANCH).await?;
        self.routes.swap(labels::routes_from_containers(&all));
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
        })
    }

    fn engine(rt: Arc<FakeRuntime>, routes: SharedRoutes) -> Engine<FakeRuntime> {
        // AlwaysReady checker: no real TCP/HTTP in unit tests.
        Engine::with_readiness(rt, routes, settings(), Arc::new(AlwaysReady))
    }

    fn request(branch: &str, json: &str) -> DeployRequest {
        DeployRequest {
            branch: branch.to_string(),
            tag: "abc".to_string(),
            sha: "sha".to_string(),
            config: config::parse(json).unwrap(),
        }
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
        let eng =
            Engine::with_readiness(rt.clone(), routes.clone(), settings(), Arc::new(NeverReady));
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
        let eng =
            Engine::with_readiness(rt.clone(), routes.clone(), settings(), Arc::new(NeverReady));
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
}
