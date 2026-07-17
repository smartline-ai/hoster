#[derive(Debug, Clone)]
pub struct Settings {
    pub listen: String,
    pub api_listen: String,
    pub hostname_template: String,
    pub registry: String,
    pub token: String,
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
}
