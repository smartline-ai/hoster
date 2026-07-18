//! Parsing an image reference far enough to know which registry it comes from.
//!
//! This is a security boundary: a project's registry credential is sent only
//! when the host parsed here matches the credential's own registry, so a
//! private token never travels to Docker Hub on a `postgres:16` pull.

/// The registry host an image reference points at, applying Docker's rules:
/// the first path segment is the host only if it looks like one (contains `.`
/// or `:`, or is exactly `localhost`). Everything else is Docker Hub.
pub fn registry_host(image: &str) -> String {
    const DOCKER_HUB: &str = "docker.io";
    let Some((first, _rest)) = image.split_once('/') else {
        return DOCKER_HUB.to_string();
    };
    if first.is_empty() {
        return DOCKER_HUB.to_string();
    }
    if first.contains('.') || first.contains(':') || first == "localhost" {
        first.to_ascii_lowercase()
    } else {
        DOCKER_HUB.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_is_docker_hub() {
        assert_eq!(registry_host("postgres"), "docker.io");
        assert_eq!(registry_host("postgres:16"), "docker.io");
    }

    #[test]
    fn namespaced_name_is_docker_hub() {
        assert_eq!(registry_host("library/postgres"), "docker.io");
        assert_eq!(registry_host("bitnami/redis:7"), "docker.io");
    }

    #[test]
    fn dotted_first_segment_is_the_host() {
        assert_eq!(registry_host("ghcr.io/org/app"), "ghcr.io");
        assert_eq!(registry_host("ghcr.io/org/app:v1"), "ghcr.io");
        assert_eq!(
            registry_host("registry.gitlab.com/g/p/img"),
            "registry.gitlab.com"
        );
    }

    #[test]
    fn host_with_port_keeps_the_port() {
        assert_eq!(
            registry_host("registry.internal:5000/app"),
            "registry.internal:5000"
        );
    }

    #[test]
    fn localhost_is_a_host_without_a_dot() {
        assert_eq!(registry_host("localhost/app"), "localhost");
        assert_eq!(registry_host("localhost:5000/app:tag"), "localhost:5000");
    }

    #[test]
    fn digest_refs_parse_the_same() {
        assert_eq!(registry_host("ghcr.io/org/app@sha256:abc123"), "ghcr.io");
        assert_eq!(registry_host("postgres@sha256:abc123"), "docker.io");
    }

    #[test]
    fn host_is_lowercased() {
        assert_eq!(registry_host("GHCR.IO/org/app"), "ghcr.io");
    }

    #[test]
    fn empty_and_malformed_refs_do_not_panic() {
        assert_eq!(registry_host(""), "docker.io");
        assert_eq!(registry_host("/leading-slash"), "docker.io");
    }
}
