//! Static routes file.
//!
//! Scaffolding so the proxy is runnable before the deploy engine exists.
//! The deploy engine builds `RoutingTable` directly; this module is deleted
//! when it lands.

use anyhow::Context;
use serde::Deserialize;

use crate::routing::{Route, RouteState, RoutingTable};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutesFile {
    #[serde(default)]
    pub routes: Vec<RouteEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    pub host: String,
    pub upstream: String,
    #[serde(default)]
    pub starting: bool,
}

pub fn parse(toml_text: &str) -> anyhow::Result<RoutingTable> {
    // `anyhow::Error::to_string()` only prints the outer context, not the
    // wrapped source — folding `e` into the message keeps the underlying
    // toml/serde detail (e.g. which field was unrecognized) visible to callers
    // that just call `.to_string()`, as our own tests do.
    let file: RoutesFile = toml::from_str(toml_text)
        .map_err(|e| anyhow::anyhow!("routes file is not valid TOML: {e}"))?;

    let mut table = RoutingTable::new();
    for entry in file.routes {
        let upstream = entry.upstream.parse().with_context(|| {
            format!(
                "route {}: upstream {:?} is not a host:port address",
                entry.host, entry.upstream
            )
        })?;
        let state = if entry.starting {
            RouteState::Starting
        } else {
            RouteState::Ready
        };
        table.insert(entry.host, Route { upstream, state });
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::RouteState;

    #[test]
    fn parses_a_ready_route() {
        let table = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"
            "#,
        )
        .unwrap();

        let route = table.lookup("backend-branch1.dev.example.com").unwrap();
        assert_eq!(route.upstream.to_string(), "127.0.0.1:8080");
        assert_eq!(route.state, RouteState::Ready);
    }

    #[test]
    fn starting_flag_maps_to_starting_state() {
        let table = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"
            starting = true
            "#,
        )
        .unwrap();

        assert_eq!(
            table
                .lookup("backend-branch1.dev.example.com")
                .unwrap()
                .state,
            RouteState::Starting
        );
    }

    #[test]
    fn parses_many_routes() {
        let table = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"

            [[routes]]
            host = "frontend-branch1.dev.example.com"
            upstream = "127.0.0.1:3000"
            "#,
        )
        .unwrap();

        assert_eq!(table.len(), 2);
    }

    #[test]
    fn empty_file_is_an_empty_table() {
        let table = parse("").unwrap();
        assert!(table.is_empty());
    }

    #[test]
    fn bad_upstream_is_an_error_naming_the_host() {
        let err = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "not-a-socket-address"
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("backend-branch1.dev.example.com"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err = parse(
            r#"
            [[routes]]
            host = "backend-branch1.dev.example.com"
            upstream = "127.0.0.1:8080"
            tls = true
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("tls"), "got: {err}");
    }
}
