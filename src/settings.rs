#[derive(Debug, Clone)]
pub struct Settings {
    pub listen: String,
    pub api_listen: String,
    pub hostname_template: String,
    pub registry: String,
    pub token: String,
    pub dashboard_password: Option<String>,
}

/// Turn an arbitrary git branch into a DNS label: lowercase, non-alphanumeric
/// runs collapsed to single hyphens, trimmed, capped at 63 chars. Not
/// reversible and never reversed — branch identity flows forward only.
pub fn sanitize_branch(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_hyphen = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_hyphen = false;
        } else if !prev_hyphen {
            out.push('-');
            prev_hyphen = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed
        .chars()
        .take(63)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

/// Fill `{service}` and `{branch}` in the operator hostname template.
pub fn hostname_for(template: &str, service: &str, branch: &str) -> String {
    template
        .replace("{service}", service)
        .replace("{branch}", branch)
}

/// Sample values substituted for the placeholders when validating a template.
/// Short and legal, so any length or character failure the check reports comes
/// from the operator's own text rather than from the sample.
const SAMPLE_SERVICE: &str = "svc";
const SAMPLE_BRANCH: &str = "br";

/// Check that a hostname template is usable before storing it.
///
/// Requires `{branch}` — without it every branch of the project resolves to one
/// hostname and each deploy silently displaces the previous. `{service}` is
/// optional: `{branch}.demo.example.com` is a legitimate single-service pattern.
pub fn validate_hostname_template(template: &str) -> Result<(), String> {
    if template.is_empty() {
        return Err("hostname template must not be empty".to_string());
    }
    if !template.contains("{branch}") {
        return Err(
            "hostname template must contain {branch}, or every branch of the project \
would resolve to the same hostname"
                .to_string(),
        );
    }
    // A TLS wildcard matches exactly one label, so every placeholder must sit
    // in the first label for `*.<rest>` to cover the hostnames produced here.
    let first_label = template.split('.').next().unwrap_or("");
    let rest = &template[first_label.len().min(template.len())..];
    if rest.contains('{') {
        return Err(
            "every placeholder must be in the hostname template's first label, \
because a TLS wildcard certificate matches only one label"
                .to_string(),
        );
    }
    let sample = hostname_for(template, SAMPLE_SERVICE, SAMPLE_BRANCH);
    validate_dns_name(&sample)
}

/// Validate a concrete hostname: total length, label lengths, and the
/// characters permitted in a DNS label.
fn validate_dns_name(name: &str) -> Result<(), String> {
    if name.len() > 253 {
        return Err(format!(
            "hostname {name:?} is {} characters; the maximum is 253",
            name.len()
        ));
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err(format!(
                "hostname {name:?} has an empty label (check for a doubled or trailing '.')"
            ));
        }
        if label.len() > 63 {
            return Err(format!(
                "label {label:?} is {} characters; the maximum is 63",
                label.len()
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("label {label:?} must not start or end with '-'"));
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(format!(
                "label {label:?} contains {bad:?}; only lowercase letters, digits, and '-' are allowed"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_slashes_and_case() {
        assert_eq!(sanitize_branch("feature/JIRA-123"), "feature-jira-123");
    }

    #[test]
    fn collapses_runs_and_trims() {
        assert_eq!(sanitize_branch("--a__b//c--"), "a-b-c");
    }

    #[test]
    fn builds_hostname() {
        assert_eq!(
            hostname_for("{service}-{branch}.dev.example.com", "backend", "b1"),
            "backend-b1.dev.example.com"
        );
    }

    #[test]
    fn accepts_a_normal_template() {
        assert!(validate_hostname_template("{service}-{branch}.dev.example.com").is_ok());
    }

    #[test]
    fn accepts_a_template_without_service() {
        assert!(validate_hostname_template("{branch}.demo.example.com").is_ok());
    }

    #[test]
    fn rejects_an_empty_template() {
        assert!(validate_hostname_template("").is_err());
    }

    #[test]
    fn rejects_a_template_without_branch() {
        let err = validate_hostname_template("{service}.dev.example.com").unwrap_err();
        assert!(
            err.contains("{branch}"),
            "message should name the missing placeholder: {err}"
        );
    }

    #[test]
    fn rejects_placeholders_spanning_two_labels() {
        // A TLS wildcard matches one label, so this could never be covered by
        // a certificate for *.dev.example.com.
        let err = validate_hostname_template("{branch}.{service}.dev.example.com").unwrap_err();
        assert!(
            err.contains("first label"),
            "message should explain the one-label rule: {err}"
        );
    }

    #[test]
    fn rejects_a_placeholder_outside_the_first_label() {
        assert!(validate_hostname_template("api.{branch}.dev.example.com").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        assert!(validate_hostname_template("{branch}.Dev.Example.com").is_err());
    }

    #[test]
    fn rejects_an_underscore() {
        assert!(validate_hostname_template("{branch}.dev_example.com").is_err());
    }

    #[test]
    fn rejects_an_empty_label() {
        assert!(validate_hostname_template("{branch}..example.com").is_err());
    }

    #[test]
    fn rejects_a_leading_or_trailing_hyphen_in_a_label() {
        assert!(validate_hostname_template("{branch}.-example.com").is_err());
        assert!(validate_hostname_template("{branch}.example-.com").is_err());
    }

    #[test]
    fn rejects_an_over_long_label() {
        let long = "a".repeat(64);
        assert!(validate_hostname_template(&format!("{{branch}}.{long}.com")).is_err());
    }

    #[test]
    fn accepts_a_label_of_exactly_63() {
        let ok = "a".repeat(63);
        assert!(validate_hostname_template(&format!("{{branch}}.{ok}.com")).is_ok());
    }
}
