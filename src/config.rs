use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    pub project: String,
    #[serde(default)]
    pub ttl: Option<String>,
    pub services: BTreeMap<String, Service>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub expose: Option<Expose>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expose {
    pub port: u16,
    #[serde(default)]
    pub subdomain: Option<String>,
    #[serde(default)]
    pub health: Option<String>,
}

pub fn parse(json: &str) -> anyhow::Result<DeployConfig> {
    serde_json::from_str(json).map_err(|e| anyhow::anyhow!("invalid hoster.json: {e}"))
}

/// Validate structural rules that serde cannot express. Returns a human
/// message on the first violation.
pub fn validate(cfg: &DeployConfig) -> Result<(), String> {
    if cfg.services.is_empty() {
        return Err("config must define at least one service".to_string());
    }
    for (name, svc) in &cfg.services {
        if !is_dns_label(name) {
            return Err(format!(
                "service name {name:?} must be a DNS label (lowercase letters, digits, hyphens; not leading/trailing hyphen)"
            ));
        }
        if let Some(expose) = &svc.expose {
            if expose.port == 0 {
                return Err(format!("service {name:?}: expose.port must be non-zero"));
            }
            if let Some(sub) = &expose.subdomain
                && !is_dns_label(sub)
            {
                return Err(format!(
                    "service {name:?}: expose.subdomain {sub:?} must be a DNS label (lowercase letters, digits, hyphens; not leading/trailing hyphen)"
                ));
            }
        }
    }
    Ok(())
}

/// RFC 1123 label: 1–63 chars, lowercase alphanumeric and hyphen, no leading
/// or trailing hyphen.
pub(crate) fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(json: &str) -> anyhow::Result<DeployConfig> {
        parse(json)
    }

    #[test]
    fn parses_minimal() {
        let c = cfg(r#"{"project":"p","services":{"backend":{"image":"img"}}}"#).unwrap();
        assert_eq!(c.project, "p");
        assert!(c.services["backend"].expose.is_none());
        assert!(c.services["backend"].env.is_empty());
    }

    #[test]
    fn parses_exposed_service() {
        let c = cfg(r#"{"project":"p","services":{"backend":{"image":"img","expose":{"port":8080,"health":"/h"}}}}"#).unwrap();
        let e = c.services["backend"].expose.as_ref().unwrap();
        assert_eq!(e.port, 8080);
        assert_eq!(e.health.as_deref(), Some("/h"));
    }

    #[test]
    fn ttl_is_accepted_and_ignored() {
        let c = cfg(r#"{"project":"p","ttl":"72h","services":{"a":{"image":"i"}}}"#).unwrap();
        assert_eq!(c.ttl.as_deref(), Some("72h"));
    }

    #[test]
    fn unknown_field_rejected() {
        let err = cfg(r#"{"project":"p","services":{"a":{"image":"i","tls":true}}}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("tls"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_services() {
        let c = cfg(r#"{"project":"p","services":{}}"#).unwrap();
        assert!(validate(&c).unwrap_err().contains("service"));
    }

    #[test]
    fn validate_rejects_bad_service_name() {
        let c = cfg(r#"{"project":"p","services":{"Bad_Name":{"image":"i"}}}"#).unwrap();
        assert!(validate(&c).unwrap_err().contains("Bad_Name"));
    }

    #[test]
    fn validate_rejects_zero_port() {
        let c =
            cfg(r#"{"project":"p","services":{"a":{"image":"i","expose":{"port":0}}}}"#).unwrap();
        assert!(validate(&c).unwrap_err().contains("port"));
    }

    #[test]
    fn validate_accepts_good_config() {
        let c =
            cfg(r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080}}}}"#)
                .unwrap();
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn validate_accepts_a_good_subdomain() {
        let c = cfg(
            r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080,"subdomain":"api-v2"}}}}"#,
        )
        .unwrap();
        assert!(validate(&c).is_ok());
    }

    #[test]
    fn validate_rejects_an_uppercase_subdomain() {
        let c = cfg(
            r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080,"subdomain":"API"}}}}"#,
        )
        .unwrap();
        let err = validate(&c).unwrap_err();
        assert!(err.contains("subdomain"), "got: {err}");
        assert!(err.contains("API"), "got: {err}");
    }

    #[test]
    fn validate_rejects_a_subdomain_with_a_leading_hyphen() {
        let c = cfg(
            r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080,"subdomain":"-api"}}}}"#,
        )
        .unwrap();
        assert!(validate(&c).unwrap_err().contains("subdomain"));
    }

    #[test]
    fn validate_rejects_a_subdomain_with_a_trailing_hyphen() {
        let c = cfg(
            r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080,"subdomain":"api-"}}}}"#,
        )
        .unwrap();
        assert!(validate(&c).unwrap_err().contains("subdomain"));
    }

    #[test]
    fn validate_rejects_an_empty_subdomain() {
        let c = cfg(
            r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080,"subdomain":""}}}}"#,
        )
        .unwrap();
        assert!(validate(&c).unwrap_err().contains("subdomain"));
    }

    #[test]
    fn validate_rejects_an_over_long_subdomain() {
        let long = "a".repeat(64);
        let c = cfg(&format!(
            r#"{{"project":"p","services":{{"backend":{{"image":"i","expose":{{"port":8080,"subdomain":"{long}"}}}}}}}}"#
        ))
        .unwrap();
        assert!(validate(&c).unwrap_err().contains("subdomain"));
    }

    #[test]
    fn validate_rejects_an_underscore_in_a_subdomain() {
        let c = cfg(
            r#"{"project":"p","services":{"backend":{"image":"i","expose":{"port":8080,"subdomain":"api_v2"}}}}"#,
        )
        .unwrap();
        assert!(validate(&c).unwrap_err().contains("subdomain"));
    }

    #[test]
    fn validate_accepts_a_subdomain_of_exactly_63() {
        let ok = "a".repeat(63);
        let c = cfg(&format!(
            r#"{{"project":"p","services":{{"backend":{{"image":"i","expose":{{"port":8080,"subdomain":"{ok}"}}}}}}}}"#
        ))
        .unwrap();
        assert!(validate(&c).is_ok());
    }
}
