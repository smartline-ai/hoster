use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub env: Vec<String>,
    pub network: String,
    pub network_alias: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningContainer {
    pub id: String,
    pub name: String,
    pub ip: Option<String>,
    pub labels: BTreeMap<String, String>,
}

#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    async fn create_network(
        &self,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> anyhow::Result<()>;
    async fn remove_network(&self, name: &str) -> anyhow::Result<()>;
    async fn pull_image(&self, image: &str) -> anyhow::Result<()>;
    async fn run(&self, spec: &ContainerSpec) -> anyhow::Result<RunningContainer>;
    async fn inspect(&self, id: &str) -> anyhow::Result<RunningContainer>;
    async fn remove_container(&self, id: &str) -> anyhow::Result<()>;
    async fn list_by_label(&self, label_key: &str) -> anyhow::Result<Vec<RunningContainer>>;
}

/// In-memory `ContainerRuntime` for tests. Public so engine and api tests
/// share one faithful fake instead of three drifting mocks.
#[derive(Default)]
pub struct FakeRuntime {
    inner: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    networks: Vec<String>,
    containers: Vec<RunningContainer>,
    /// Env passed to `run`, keyed by container name — env isn't part of
    /// `RunningContainer`, so capture it here for test assertions.
    env: BTreeMap<String, Vec<String>>,
    next: u32,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of containers currently "running" — for test assertions.
    pub fn container_count(&self) -> usize {
        self.inner.lock().unwrap().containers.len()
    }

    /// The env `run` last received for a container name — for test assertions.
    pub fn env_of(&self, container_name: &str) -> Option<Vec<String>> {
        self.inner.lock().unwrap().env.get(container_name).cloned()
    }
}

#[async_trait]
impl ContainerRuntime for FakeRuntime {
    async fn create_network(
        &self,
        name: &str,
        _labels: &BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut s = self.inner.lock().unwrap();
        if !s.networks.iter().any(|n| n == name) {
            s.networks.push(name.to_string());
        }
        Ok(())
    }

    async fn remove_network(&self, name: &str) -> anyhow::Result<()> {
        self.inner.lock().unwrap().networks.retain(|n| n != name);
        Ok(())
    }

    async fn pull_image(&self, _image: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn run(&self, spec: &ContainerSpec) -> anyhow::Result<RunningContainer> {
        let mut s = self.inner.lock().unwrap();
        if !s.networks.contains(&spec.network) {
            anyhow::bail!("network {} does not exist", spec.network);
        }
        s.next += 1;
        let n = s.next;
        let c = RunningContainer {
            id: format!("fake-{n}"),
            name: spec.name.clone(),
            ip: Some(format!("10.42.{}.{}", n / 256, n % 256)),
            labels: spec.labels.clone(),
        };
        s.env.insert(spec.name.clone(), spec.env.clone());
        s.containers.push(c.clone());
        Ok(c)
    }

    async fn inspect(&self, id: &str) -> anyhow::Result<RunningContainer> {
        self.inner
            .lock()
            .unwrap()
            .containers
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such container {id}"))
    }

    async fn remove_container(&self, id: &str) -> anyhow::Result<()> {
        self.inner.lock().unwrap().containers.retain(|c| c.id != id);
        Ok(())
    }

    async fn list_by_label(&self, label_key: &str) -> anyhow::Result<Vec<RunningContainer>> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .containers
            .iter()
            .filter(|c| c.labels.contains_key(label_key))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, branch: &str) -> ContainerSpec {
        let mut labels = BTreeMap::new();
        labels.insert("hoster.branch".to_string(), branch.to_string());
        ContainerSpec {
            name: name.to_string(),
            image: "img".to_string(),
            env: vec![],
            network: format!("hoster-{branch}"),
            network_alias: name.to_string(),
            labels,
        }
    }

    #[tokio::test]
    async fn run_then_inspect_returns_an_ip() {
        let rt = FakeRuntime::new();
        rt.create_network("hoster-b1", &BTreeMap::new())
            .await
            .unwrap();
        let c = rt.run(&spec("b1-backend", "b1")).await.unwrap();
        assert!(c.ip.is_some());
        let again = rt.inspect(&c.id).await.unwrap();
        assert_eq!(again.ip, c.ip);
    }

    #[tokio::test]
    async fn distinct_containers_get_distinct_ips() {
        let rt = FakeRuntime::new();
        rt.create_network("hoster-b1", &BTreeMap::new())
            .await
            .unwrap();
        let a = rt.run(&spec("b1-a", "b1")).await.unwrap();
        let b = rt.run(&spec("b1-b", "b1")).await.unwrap();
        assert_ne!(a.ip, b.ip);
    }

    #[tokio::test]
    async fn list_by_label_filters() {
        let rt = FakeRuntime::new();
        rt.create_network("hoster-b1", &BTreeMap::new())
            .await
            .unwrap();
        rt.run(&spec("b1-a", "b1")).await.unwrap();
        let found = rt.list_by_label("hoster.branch").await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].labels["hoster.branch"], "b1");
    }

    #[tokio::test]
    async fn remove_container_then_absent() {
        let rt = FakeRuntime::new();
        rt.create_network("hoster-b1", &BTreeMap::new())
            .await
            .unwrap();
        let c = rt.run(&spec("b1-a", "b1")).await.unwrap();
        rt.remove_container(&c.id).await.unwrap();
        assert!(rt.list_by_label("hoster.branch").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn run_without_network_errors() {
        let rt = FakeRuntime::new();
        assert!(rt.run(&spec("b1-a", "b1")).await.is_err());
    }
}
