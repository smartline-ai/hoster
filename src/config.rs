use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployConfig {
    pub project: String,
    #[serde(default)]
    pub ttl: Option<String>,
    pub services: BTreeMap<String, Service>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub image: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub expose: Option<Expose>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
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
        if let Some(expose) = &svc.expose
            && expose.port == 0
        {
            return Err(format!("service {name:?}: expose.port must be non-zero"));
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
}
