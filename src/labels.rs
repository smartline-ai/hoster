use crate::routing::{RouteState, RoutingTable};
use crate::runtime::RunningContainer;

pub const BRANCH: &str = "hoster.branch";
pub const SERVICE: &str = "hoster.service";
pub const PORT: &str = "hoster.port";
pub const HOSTNAME: &str = "hoster.hostname";

/// Rebuild a routing table from running containers. A container is routed only
/// if it carries a hostname label, a parseable port label, and a known IP —
/// exactly the exposed services. Everything else (internal services, or
/// containers whose IP could not be resolved) is skipped, never guessed.
pub fn routes_from_containers(containers: &[RunningContainer]) -> RoutingTable {
    let mut table = RoutingTable::new();
    for c in containers {
        let (Some(hostname), Some(port_str), Some(ip)) =
            (c.labels.get(HOSTNAME), c.labels.get(PORT), c.ip.as_ref())
        else {
            continue;
        };
        let Ok(port) = port_str.parse::<u16>() else {
            tracing::warn!(container = %c.name, port = %port_str, "unparseable port label, skipping");
            continue;
        };
        let Ok(upstream) = format!("{ip}:{port}").parse() else {
            tracing::warn!(container = %c.name, %ip, "unparseable container ip, skipping");
            continue;
        };
        table.insert(
            hostname.clone(),
            crate::routing::Route {
                upstream,
                state: RouteState::Ready,
            },
        );
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn container(labels: &[(&str, &str)], ip: Option<&str>) -> RunningContainer {
        RunningContainer {
            id: "id".to_string(),
            name: "n".to_string(),
            ip: ip.map(str::to_string),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn exposed_container_becomes_a_ready_route() {
        let c = container(
            &[
                (BRANCH, "b1"),
                (SERVICE, "backend"),
                (HOSTNAME, "backend-b1.dev.example.com"),
                (PORT, "8080"),
            ],
            Some("10.42.0.5"),
        );
        let table = routes_from_containers(&[c]);
        let r = table.lookup("backend-b1.dev.example.com").unwrap();
        assert_eq!(r.upstream.to_string(), "10.42.0.5:8080");
        assert_eq!(r.state, RouteState::Ready);
    }

    #[test]
    fn container_without_hostname_is_not_routed() {
        let c = container(&[(BRANCH, "b1"), (SERVICE, "postgres")], Some("10.42.0.6"));
        assert!(routes_from_containers(&[c]).is_empty());
    }

    #[test]
    fn container_without_ip_is_skipped() {
        let c = container(
            &[
                (BRANCH, "b1"),
                (SERVICE, "backend"),
                (HOSTNAME, "h"),
                (PORT, "8080"),
            ],
            None,
        );
        assert!(routes_from_containers(&[c]).is_empty());
    }

    #[test]
    fn bad_port_label_is_skipped() {
        let c = container(
            &[(BRANCH, "b1"), (HOSTNAME, "h"), (PORT, "notaport")],
            Some("10.42.0.7"),
        );
        assert!(routes_from_containers(&[c]).is_empty());
    }
}
