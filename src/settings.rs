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
///
/// The finished hostname is guaranteed to be a valid DNS name: if substituting
/// normally would make the first label exceed 63 characters, the branch
/// portion of that label is shortened to make room for a deterministic 7
/// character suffix (`-` + 6 lowercase hex characters derived from a hash of
/// the *full* original branch), so two long branches that share a prefix
/// still produce different hostnames instead of colliding. Templates whose
/// static parts alone already fill the label are truncated to 63 characters
/// as a last resort. The common case — everything already fits — is
/// returned untouched.
pub fn hostname_for(template: &str, service: &str, branch: &str) -> String {
    let full = template
        .replace("{service}", service)
        .replace("{branch}", branch);
    let first_label = full.split('.').next().unwrap_or("");
    if first_label.len() <= 63 {
        return full;
    }
    let rest = &full[first_label.len()..];
    format!("{}{rest}", shorten_first_label(template, service, branch))
}

/// Build a first label that fits in 63 characters by shortening the branch
/// portion, appending a deterministic hash suffix so truncated branches that
/// share a prefix don't collide. See [`hostname_for`].
fn shorten_first_label(template: &str, service: &str, branch: &str) -> String {
    let template_first_label = template.split('.').next().unwrap_or("");
    let with_service = template_first_label.replace("{service}", service);

    let Some((before, after)) = with_service.split_once("{branch}") else {
        // No {branch} placeholder in the first label to shorten (shouldn't
        // happen for a template that passed `validate_hostname_template`,
        // but fall back to a hard truncation rather than an invalid label).
        let with_branch = template_first_label
            .replace("{branch}", branch)
            .replace("{service}", service);
        let mut label: String = with_branch.chars().take(63).collect();
        while label.ends_with('-') {
            label.pop();
        }
        return label;
    };

    let suffix = branch_hash_suffix(branch);
    let budget = 63usize.saturating_sub(before.len() + after.len());
    if budget == 0 {
        // The static (non-branch) part of the label is already >= 63 chars;
        // there's no room for any branch or suffix at all.
        let mut label: String = format!("{before}{after}").chars().take(63).collect();
        while label.ends_with('-') {
            label.pop();
        }
        return label;
    }

    let prefix_budget = budget.saturating_sub(suffix.len());
    let mut branch_prefix: String = branch.chars().take(prefix_budget).collect();
    while branch_prefix.ends_with('-') {
        branch_prefix.pop();
    }

    let mut label = format!("{before}{branch_prefix}{suffix}{after}");
    if label.len() > 63 {
        // Only reachable when `budget` was too small to fit the suffix
        // (budget < 7); fall back to a hard truncation.
        label = label.chars().take(63).collect();
        while label.ends_with('-') {
            label.pop();
        }
    }
    label
}

/// FNV-1a, 64-bit. Deliberately not `std::collections::hash_map::DefaultHasher`:
/// that hasher's output is not guaranteed stable across Rust versions, so a
/// compiler upgrade could silently change every existing branch's hostname.
/// FNV-1a's algorithm is fixed forever, so the same branch always hashes to
/// the same suffix.
fn fnv1a_hash(input: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET_BASIS;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Deterministic 7-character suffix (`-` + 6 lowercase hex chars) derived
/// from the full original branch, used when a branch must be truncated to
/// fit a DNS label. Two branches that share a long common prefix still hash
/// differently (almost certainly), so truncation cannot make them collide.
fn branch_hash_suffix(branch: &str) -> String {
    let hash = fnv1a_hash(branch);
    format!("-{:06x}", hash & 0xff_ffff)
}

/// Sample values substituted for the placeholders when validating a template.
/// These exercise only the template's static parts — literal characters,
/// dots, and placeholder placement — not real-world lengths, so they are
/// deliberately short. Runtime length is guaranteed separately: `hostname_for`
/// shortens the branch portion so the finished hostname's first label never
/// exceeds the 63-character DNS limit, however long the branch or service is.
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
    if !template.contains('.') {
        return Err(
            "hostname template must include a parent domain (e.g. \"{branch}.example.com\"); \
without one there is no parent domain for a wildcard certificate to cover"
                .to_string(),
        );
    }
    // A TLS wildcard matches exactly one label, so every placeholder must sit
    // in the first label for `*.<rest>` to cover the hostnames produced here.
    let first_label = template.split('.').next().unwrap_or("");
    if first_label.is_empty() {
        return Err("hostname template has an empty label (check for a leading '.')".to_string());
    }
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

    #[test]
    fn rejects_a_single_label_template() {
        let err = validate_hostname_template("{branch}").unwrap_err();
        assert!(
            err.contains("parent domain"),
            "message should explain a parent domain is required: {err}"
        );
    }

    #[test]
    fn rejects_a_leading_dot_with_an_empty_label_message() {
        let err = validate_hostname_template(".{branch}.dev.example.com").unwrap_err();
        assert!(
            err.contains("empty label"),
            "message should name the actual defect (empty label), not placeholder \
placement: {err}"
        );
        assert!(
            !err.contains("placeholder"),
            "message should not blame placeholder placement: {err}"
        );
    }

    #[test]
    fn hostname_for_returns_short_hostnames_unchanged() {
        assert_eq!(
            hostname_for("{service}-{branch}.dev.example.com", "backend", "b1"),
            "backend-b1.dev.example.com"
        );
    }

    #[test]
    fn hostname_for_shortens_an_overflowing_first_label_to_63() {
        let branch = "b".repeat(63);
        let host = hostname_for("{service}-{branch}.dev.example.com", "backend", &branch);
        let first_label = host.split('.').next().unwrap();
        assert_eq!(
            first_label.len(),
            63,
            "first label should be exactly 63: {host}"
        );
        assert!(
            host.ends_with(".dev.example.com"),
            "rest of the hostname must stay intact: {host}"
        );
        assert!(
            first_label.starts_with("backend-"),
            "service portion should be preserved: {host}"
        );
    }

    #[test]
    fn hostname_for_disambiguates_branches_sharing_a_long_prefix() {
        let branch_a = format!("{}{}", "x".repeat(50), "a".repeat(13));
        let branch_b = format!("{}{}", "x".repeat(50), "b".repeat(13));
        assert_eq!(branch_a.len(), 63);
        assert_eq!(branch_b.len(), 63);

        let host_a = hostname_for("{service}-{branch}.dev.example.com", "backend", &branch_a);
        let host_b = hostname_for("{service}-{branch}.dev.example.com", "backend", &branch_b);
        assert_ne!(
            host_a, host_b,
            "branches sharing a 50-char prefix must not collide"
        );
    }

    #[test]
    fn hostname_for_is_deterministic() {
        let branch = "c".repeat(63);
        let host1 = hostname_for("{service}-{branch}.dev.example.com", "backend", &branch);
        let host2 = hostname_for("{service}-{branch}.dev.example.com", "backend", &branch);
        assert_eq!(host1, host2);
    }

    #[test]
    fn hostname_for_truncated_label_has_no_leading_or_trailing_hyphen() {
        // Constructed so the 48-char truncation boundary lands right after a
        // hyphen in the branch, which must be stripped before the suffix.
        let branch = format!("{}-{}", "a".repeat(47), "b".repeat(15));
        assert_eq!(branch.len(), 63);
        let host = hostname_for("{service}-{branch}.dev.example.com", "backend", &branch);
        let first_label = host.split('.').next().unwrap();
        assert!(
            !first_label.starts_with('-') && !first_label.ends_with('-'),
            "truncated label must not start or end with '-': {first_label:?}"
        );
        assert!(first_label.len() <= 63);
    }
}
