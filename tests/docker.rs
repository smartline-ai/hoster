use std::collections::BTreeMap;

use hoster::docker::DockerRuntime;
use hoster::runtime::{ContainerRuntime, ContainerSpec};

/// Connect, or skip the test if no daemon is reachable.
///
/// `DockerRuntime::connect()` itself can fail (e.g. no socket file at all --
/// the common case when a Docker Desktop/OrbStack-style daemon is simply not
/// running), not just `ping()`. Print SKIP on either failure so the test
/// never fails silently on a machine without Docker.
async fn runtime_or_skip() -> Option<DockerRuntime> {
    let rt = match DockerRuntime::connect() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("SKIP: no reachable Docker daemon ({e})");
            return None;
        }
    };
    match rt.ping().await {
        Ok(()) => Some(rt),
        Err(e) => {
            eprintln!("SKIP: no reachable Docker daemon ({e})");
            None
        }
    }
}

fn labels(branch: &str, service: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("hoster.branch".to_string(), branch.to_string());
    m.insert("hoster.service".to_string(), service.to_string());
    m
}

// NOTE: `ContainerSpec` has no `cmd` field (Task 3 type, unchanged by this
// task). `alpine:3.20` with no command exits immediately after start, so we
// don't assert on `c.ip` here -- inspecting a just-exited container may
// legitimately report no IP. The test still exercises create_network,
// pull_image, run, inspect (via run), list_by_label, remove_container and
// remove_network against a live daemon. If a future task needs a stable IP
// for a live-running container, extend `ContainerSpec` with an optional
// `cmd: Option<Vec<String>>` (threaded into `Config.cmd` in
// `DockerRuntime::run`, ignored by `FakeRuntime`) and set it to
// `Some(vec!["sleep".into(), "30".into()])` here.
#[tokio::test]
async fn network_run_inspect_and_cleanup() {
    let Some(rt) = runtime_or_skip().await else {
        return;
    };
    let net = "hoster-itest-1";
    let _ = rt.remove_network(net).await; // clean slate

    rt.create_network(net, &BTreeMap::new()).await.unwrap();
    rt.pull_image("alpine:3.20").await.unwrap();

    let spec = ContainerSpec {
        name: "hoster-itest-1-web".to_string(),
        image: "alpine:3.20".to_string(),
        env: vec![],
        network: net.to_string(),
        network_alias: "web".to_string(),
        labels: labels("itest-1", "web"),
    };
    let c = rt.run(&spec).await.unwrap();
    assert_eq!(c.name, "hoster-itest-1-web");
    assert_eq!(
        c.labels.get("hoster.branch").map(String::as_str),
        Some("itest-1")
    );

    let listed = rt.list_by_label("hoster.branch").await.unwrap();
    assert!(listed.iter().any(|x| x.name == "hoster-itest-1-web"));

    rt.remove_container(&c.id).await.unwrap();
    rt.remove_network(net).await.unwrap();
}
