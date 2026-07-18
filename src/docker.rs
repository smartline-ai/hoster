//! The only module that touches bollard. Everything else speaks the
//! `ContainerRuntime` trait.

use std::collections::HashMap;

use bollard::Docker;
use bollard::auth::DockerCredentials;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, LogOutput, LogsOptions,
    NetworkingConfig, RemoveContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{EndpointSettings, HostConfig};
use bollard::network::CreateNetworkOptions;
use futures_util::TryStreamExt;

use crate::runtime::{ContainerRuntime, ContainerSpec, LogStream, RunningContainer};
use crate::secrets::RegistryCred;

pub struct DockerRuntime {
    docker: Docker,
}

impl DockerRuntime {
    /// Connect using standard Docker env (`DOCKER_HOST`, default socket).
    pub fn connect() -> anyhow::Result<Self> {
        let docker = Docker::connect_with_local_defaults()?;
        Ok(Self { docker })
    }

    /// Probe the daemon; used by main to fail fast and by tests to self-skip.
    pub async fn ping(&self) -> anyhow::Result<()> {
        self.docker.ping().await?;
        Ok(())
    }
}

fn to_running(
    name: String,
    id: String,
    labels: HashMap<String, String>,
    ip: Option<String>,
) -> RunningContainer {
    RunningContainer {
        id,
        name,
        ip,
        labels: labels.into_iter().collect(),
    }
}

#[async_trait::async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn create_network(
        &self,
        name: &str,
        labels: &std::collections::BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        self.docker
            .create_network(CreateNetworkOptions {
                name: name.to_string(),
                driver: "bridge".to_string(),
                labels: labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    async fn remove_network(&self, name: &str) -> anyhow::Result<()> {
        self.docker.remove_network(name).await?;
        Ok(())
    }

    async fn pull_image(&self, image: &str, cred: Option<&RegistryCred>) -> anyhow::Result<()> {
        let auth = cred.map(|c| DockerCredentials {
            username: Some(c.username.clone()),
            password: Some(c.password.clone()),
            serveraddress: Some(c.registry.clone()),
            ..Default::default()
        });
        self.docker
            .create_image(
                Some(CreateImageOptions {
                    from_image: image.to_string(),
                    ..Default::default()
                }),
                None,
                auth,
            )
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }

    async fn run(&self, spec: &ContainerSpec) -> anyhow::Result<RunningContainer> {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            spec.network.clone(),
            EndpointSettings {
                aliases: Some(vec![spec.network_alias.clone()]),
                ..Default::default()
            },
        );
        let config: Config<String> = Config {
            image: Some(spec.image.clone()),
            env: Some(spec.env.clone()),
            labels: Some(
                spec.labels
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            host_config: Some(HostConfig {
                ..Default::default()
            }),
            networking_config: Some(NetworkingConfig {
                endpoints_config: endpoints,
            }),
            ..Default::default()
        };
        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: spec.name.clone(),
                    platform: None,
                }),
                config,
            )
            .await?;
        self.docker
            .start_container::<String>(&created.id, None)
            .await?;
        self.inspect(&created.id).await
    }

    async fn inspect(&self, id: &str) -> anyhow::Result<RunningContainer> {
        let c = self.docker.inspect_container(id, None).await?;
        let name = c
            .name
            .clone()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();
        let labels = c
            .config
            .as_ref()
            .and_then(|cfg| cfg.labels.clone())
            .unwrap_or_default();
        let ip = c
            .network_settings
            .and_then(|ns| ns.networks)
            .and_then(|nets| nets.into_values().find_map(|ep| ep.ip_address))
            .filter(|s| !s.is_empty());
        Ok(to_running(name, id.to_string(), labels, ip))
    }

    async fn remove_container(&self, id: &str) -> anyhow::Result<()> {
        self.docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(())
    }

    async fn list_by_label(&self, label_key: &str) -> anyhow::Result<Vec<RunningContainer>> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![label_key.to_string()]);
        let summaries = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await?;
        let mut out = Vec::new();
        for s in summaries {
            if let Some(id) = s.id {
                out.push(self.inspect(&id).await?);
            }
        }
        Ok(out)
    }

    async fn logs(
        &self,
        container_id: &str,
        follow: bool,
        tail: usize,
    ) -> anyhow::Result<LogStream> {
        use futures_util::StreamExt;
        let options = LogsOptions::<String> {
            follow,
            stdout: true,
            stderr: true,
            tail: tail.to_string(),
            timestamps: false,
            ..Default::default()
        };
        let raw = self.docker.logs(container_id, Some(options));
        // A single Docker log frame can carry several `\n`-separated lines
        // (e.g. a multi-line stack trace written in one syscall), but the
        // `LogStream` contract is one decoded line per item. Split each
        // frame into its lines and flatten with `stream::iter` so the
        // output stream stays a uniform `anyhow::Result<String>` (no
        // `Either`/`left_stream`/`right_stream` needed). A stream error
        // still surfaces as exactly one `Err` item. Note: a line split
        // across two frames (rare) may surface as two items — an accepted
        // limitation for a live tail.
        let mapped = raw.flat_map(|chunk: Result<LogOutput, bollard::errors::Error>| {
            let items: Vec<anyhow::Result<String>> = match chunk {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.into_bytes()).into_owned();
                    let mut lines: Vec<String> = text
                        .split('\n')
                        .map(|l| l.trim_end_matches('\r').to_string())
                        .collect();
                    // split() on a trailing '\n' leaves a final empty
                    // element — drop it so a terminal newline doesn't
                    // produce a spurious empty item.
                    if lines.last().is_some_and(|s| s.is_empty()) {
                        lines.pop();
                    }
                    lines.into_iter().map(Ok).collect()
                }
                Err(e) => vec![Err(anyhow::Error::from(e))],
            };
            futures_util::stream::iter(items)
        });
        Ok(Box::pin(mapped))
    }
}
