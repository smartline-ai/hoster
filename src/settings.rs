/// Which reverse-proxy topology hoster runs in.
///
/// `Standalone` is today's behavior: hoster binds `:80`/`:443` and is the edge
/// proxy. `Nginx` puts nginx in front as the TLS-terminating edge, proxying to
/// hoster's plain HTTP listener; hoster stops binding `:443` and instead
/// generates nginx config.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProxyMode {
    Standalone,
    Nginx,
}

impl ProxyMode {
    pub fn parse(s: &str) -> anyhow::Result<ProxyMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" => Ok(ProxyMode::Standalone),
            "nginx" => Ok(ProxyMode::Nginx),
            other => anyhow::bail!(
                "unknown HOSTER_PROXY_MODE {other:?}; valid values are \"standalone\" and \"nginx\""
            ),
        }
    }
}

#[derive(Clone)]
pub struct Settings {
    pub listen: String,
    pub api_listen: String,
    pub hostname_template: String,
    pub registry: String,
    pub token: String,
    pub dashboard_password: Option<String>,
    /// Where to accept HTTPS. `None` disables TLS entirely: no listener, no
    /// renewal loop, and no issuance, so upgrading an existing install
    /// changes nothing until it is set.
    pub https_listen: Option<String>,
    /// Root directory of the certificate store.
    pub cert_dir: String,
    /// The box's public IP, published as the wildcard A record's target.
    /// Required once any non-manual DNS provider is configured.
    pub public_ip: Option<String>,
    /// Reverse-proxy topology. `Standalone` (default) = hoster is the edge.
    pub proxy_mode: ProxyMode,
    /// nginx mode only: the config file hoster generates.
    pub nginx_conf_path: String,
    /// nginx mode only: the shell command hoster runs to reload nginx after a
    /// successful `nginx -t`.
    pub nginx_reload_cmd: String,
}

impl Settings {
    /// The URL scheme hoster advertises for the environments it publishes.
    ///
    /// Every public URL hoster reports — the `{{url.*}}` values injected into
    /// containers, the API's deploy/deployments responses, and the dashboard's
    /// links — must use the scheme a browser will actually reach the
    /// environment on. TLS is terminated in one of two ways, and either one
    /// means the public scheme is `https`: hoster terminates it itself when
    /// `https_listen` is set, OR nginx terminates it on `:443` when
    /// `proxy_mode` is `Nginx` (in which case `https_listen` is unset/ignored,
    /// but the edge is still HTTPS). Hardcoding `http://` in nginx mode makes a
    /// frontend on the HTTPS edge call its backend over plain HTTP and get
    /// blocked as mixed content.
    pub fn public_scheme(&self) -> &'static str {
        if self.https_listen.is_some() || self.proxy_mode == ProxyMode::Nginx {
            "https"
        } else {
            "http"
        }
    }
}

/// A hand-written `Debug` that redacts `token` (the bearer credential that
/// authenticates the whole control API) and `dashboard_password` — the same
/// reasoning as [`crate::secrets::DnsProviderConfig`]'s impl: a derived
/// `Debug` would print both in full the moment anything logs or formats a
/// `Settings` value.
impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("listen", &self.listen)
            .field("api_listen", &self.api_listen)
            .field("hostname_template", &self.hostname_template)
            .field("registry", &self.registry)
            .field("token", &"[redacted]")
            .field(
                "dashboard_password",
                &self.dashboard_password.as_ref().map(|_| "[redacted]"),
            )
            .field("https_listen", &self.https_listen)
            .field("cert_dir", &self.cert_dir)
            .field("public_ip", &self.public_ip)
            .field("proxy_mode", &self.proxy_mode)
            .field("nginx_conf_path", &self.nginx_conf_path)
            .field("nginx_reload_cmd", &self.nginx_reload_cmd)
            .finish()
    }
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

/// Build a first label that fits in 63 bytes by shortening the branch
/// portion, appending a deterministic hash suffix so truncated branches that
/// share a prefix don't collide. See [`hostname_for`].
///
/// Unconditionally safe regardless of what `service` and `branch` contain:
/// the result is always 1-63 bytes, never starts or ends with `-`, contains
/// only `[a-z0-9-]`, and always carries the branch hash suffix (this
/// function is only ever reached when the substituted label already
/// overflowed 63 bytes, so some truncation is unavoidable; the suffix is
/// what keeps two different truncated branches from colliding, so it is the
/// one thing truncation must never sacrifice). `service` in particular is
/// *not* guaranteed to be validated by the time it reaches here, so this
/// must not assume it is short, ASCII, or DNS-safe.
fn shorten_first_label(template: &str, service: &str, branch: &str) -> String {
    let template_first_label = template.split('.').next().unwrap_or("");
    let with_service = template_first_label.replace("{service}", service);
    let suffix = branch_hash_suffix(branch);

    // No {branch} placeholder in the first label to shorten shouldn't happen
    // for a template that passed `validate_hostname_template`, but treat the
    // whole label as leading text rather than assuming it away.
    let (before, after) = with_service
        .split_once("{branch}")
        .unwrap_or((with_service.as_str(), ""));

    build_shortened_label(before, after, branch, &suffix)
}

/// Assemble `before + branch-prefix + suffix + after` into a valid DNS
/// label, operating on bytes throughout (never `char`s) so a multi-byte
/// UTF-8 character can't make the byte length exceed 63 while a char count
/// looks fine. `suffix` is reserved first and never trimmed away; `before`,
/// `branch`, and `after` are truncated (roughly in that priority) to fit
/// whatever budget remains, with any stray leading/trailing `-` left by
/// truncation stripped from the final result.
fn build_shortened_label(before: &str, after: &str, branch: &str, suffix: &str) -> String {
    const MAX: usize = 63;
    let before = dns_safe_bytes(before);
    let after = dns_safe_bytes(after);
    let branch = dns_safe_bytes(branch);
    let suffix = suffix.as_bytes();

    // The suffix is non-negotiable: reserve its budget first so nothing
    // below can crowd it out, however tight the rest of the label is.
    let text_budget = MAX.saturating_sub(suffix.len());

    let (before_len, after_len) = if before.len() + after.len() <= text_budget {
        (before.len(), after.len())
    } else {
        // Not even the literal/service text fits alongside the suffix, so
        // the branch gets nothing; `after` is sacrificed before `before`,
        // since `before` (typically the service and a separator) reads
        // first and carries more identifying information.
        let before_len = before.len().min(text_budget);
        let after_len = after.len().min(text_budget - before_len);
        (before_len, after_len)
    };
    let branch_len = branch.len().min(text_budget - before_len - after_len);

    let mut out = Vec::with_capacity(MAX);
    out.extend_from_slice(&before[..before_len]);
    out.extend_from_slice(&branch[..branch_len]);
    out.extend_from_slice(suffix);
    out.extend_from_slice(&after[..after_len]);
    debug_assert!(out.len() <= MAX);

    let mut start = 0;
    while start < out.len() && out[start] == b'-' {
        start += 1;
    }
    let mut end = out.len();
    while end > start && out[end - 1] == b'-' {
        end -= 1;
    }

    // `suffix` is 6 hex digits behind its leading `-`, and hex digits are
    // never `-`, so trimming can shrink it to a bare hex run but can never
    // erase it or leave `out` empty.
    String::from_utf8(out[start..end].to_vec()).expect("dns_safe_bytes only emits ASCII")
}

/// Keep only the bytes a DNS label may contain (`[a-z0-9-]`), dropping
/// everything else. Defense in depth for [`build_shortened_label`]: inputs
/// reaching it are expected to already be DNS-safe ASCII, but `service` in
/// particular flows in from user-supplied config that this function cannot
/// assume was validated.
fn dns_safe_bytes(s: &str) -> Vec<u8> {
    s.bytes()
        .filter(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        .collect()
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
///
/// Used both on the sample hostname a template produces and on operator-typed
/// names such as the control hostname, which becomes a certificate identifier
/// and so must be a real DNS name.
pub fn validate_hostname(name: &str) -> Result<(), String> {
    validate_dns_name(name)
}

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

/// The wildcard certificate name covering every hostname a template produces.
/// Returns `None` when the first label has no placeholder, since such a
/// template yields one fixed hostname needing no wildcard.
pub fn wildcard_base(template: &str) -> Option<String> {
    let (first, rest) = template.split_once('.')?;
    if !first.contains('{') {
        return None;
    }
    Some(format!("*.{rest}"))
}

/// The identifier set for a certificate. A wildcard does not cover its own
/// parent, so `*.dev.example.com` is paired with `dev.example.com`.
pub fn cert_identifiers(name: &str) -> Vec<String> {
    match name.strip_prefix("*.") {
        Some(parent) => vec![name.to_string(), parent.to_string()],
        None => vec![name.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(https_listen: Option<&str>) -> Settings {
        Settings {
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "reg.example.com".into(),
            token: "t".into(),
            dashboard_password: None,
            https_listen: https_listen.map(str::to_string),
            cert_dir: "/tmp/hoster-test-certs".into(),
            public_ip: None,
            proxy_mode: ProxyMode::Standalone,
            nginx_conf_path: "/etc/nginx/conf.d/hoster.conf".into(),
            nginx_reload_cmd: "systemctl reload nginx".into(),
        }
    }

    #[test]
    fn public_scheme_is_https_only_when_hoster_terminates_tls() {
        // Standalone, no https listener: hoster is a plain-HTTP edge.
        assert_eq!(settings(None).public_scheme(), "http");
        // Standalone with an https listener: hoster terminates TLS.
        assert_eq!(settings(Some("0.0.0.0:8443")).public_scheme(), "https");

        // nginx mode terminates TLS on :443 even though https_listen is unset,
        // so the public scheme must still be https.
        let mut nginx = settings(None);
        nginx.proxy_mode = ProxyMode::Nginx;
        assert_eq!(nginx.public_scheme(), "https");

        // Explicit standalone with no https listener stays http.
        let mut standalone = settings(None);
        standalone.proxy_mode = ProxyMode::Standalone;
        assert_eq!(standalone.public_scheme(), "http");
    }

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

    #[test]
    fn hostname_for_disambiguates_when_a_long_service_zeroes_the_budget() {
        // Critical: a `service` long enough that `before.len() + after.len()`
        // alone already reaches 63 must not make the branch (and its hash
        // suffix) get dropped entirely -- that would collapse every branch
        // of an over-long service into one hostname.
        let service = "s".repeat(70);
        let host_a = hostname_for(
            "{service}-{branch}.dev.example.com",
            &service,
            "branch-alpha",
        );
        let host_b = hostname_for(
            "{service}-{branch}.dev.example.com",
            &service,
            "branch-beta-totally-different",
        );
        assert_ne!(
            host_a, host_b,
            "different branches must not collapse to the same hostname just \
because the service name is long: {host_a} vs {host_b}"
        );
    }

    #[test]
    fn hostname_for_leading_branch_label_never_starts_with_a_hyphen() {
        // Critical: when {branch} opens the first label and a long service
        // zeroes the prefix budget, the label must not become the hash
        // suffix's leading '-' verbatim.
        let service = "s".repeat(61);
        let host = hostname_for("{branch}-{service}.dev.example.com", &service, "my-branch");
        let first_label = host.split('.').next().unwrap();
        assert!(
            !first_label.starts_with('-'),
            "label must not start with '-': {first_label:?}"
        );
        assert!(
            !first_label.ends_with('-'),
            "label must not end with '-': {first_label:?}"
        );
        assert!(first_label.len() <= 63);
    }

    #[test]
    fn hostname_for_non_ascii_service_stays_within_63_bytes() {
        // Important: truncation must count bytes, not chars -- each 'é' is 2
        // UTF-8 bytes, so a naive `.chars().take(63)` would silently produce
        // a 126-byte label reported as within the 63-char limit.
        let service = "é".repeat(70);
        let host = hostname_for("{service}-{branch}.dev.example.com", &service, "my-branch");
        let first_label = host.split('.').next().unwrap();
        assert!(
            first_label.len() <= 63,
            "label must be <=63 *bytes*, got {}: {first_label:?}",
            first_label.len()
        );
    }

    #[test]
    fn shorten_first_label_invariants_hold_across_length_combinations() {
        // Property-style sweep over the function under test itself: whatever
        // combination of service/branch lengths and template shape produced
        // the overflow, the label `shorten_first_label` returns must always
        // be a valid, non-empty, hash-disambiguated DNS label.
        let lengths = [0usize, 1, 30, 62, 63, 64, 100];
        let templates = [
            "{service}-{branch}.dev.example.com",
            "{branch}-{service}.dev.example.com",
        ];
        for &service_len in &lengths {
            for &branch_len in &lengths {
                let service = "s".repeat(service_len);
                let branch = "b".repeat(branch_len);
                for template in templates {
                    let label = shorten_first_label(template, &service, &branch);
                    assert!(
                        !label.is_empty(),
                        "template {template:?} service_len={service_len} \
branch_len={branch_len}: label must not be empty"
                    );
                    assert!(
                        label.len() <= 63,
                        "template {template:?} service_len={service_len} \
branch_len={branch_len}: label {label:?} is {} bytes",
                        label.len()
                    );
                    assert!(
                        !label.starts_with('-') && !label.ends_with('-'),
                        "template {template:?} service_len={service_len} \
branch_len={branch_len}: label {label:?} starts or ends with '-'"
                    );
                    assert!(
                        label
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                        "template {template:?} service_len={service_len} \
branch_len={branch_len}: label {label:?} has an invalid byte"
                    );
                }
            }
        }
    }

    #[test]
    fn wildcard_base_replaces_the_first_label() {
        assert_eq!(
            wildcard_base("{service}-{branch}.dev.example.com").as_deref(),
            Some("*.dev.example.com")
        );
        assert_eq!(
            wildcard_base("{branch}.demo.example.com").as_deref(),
            Some("*.demo.example.com")
        );
    }

    #[test]
    fn wildcard_base_is_none_without_a_placeholder() {
        assert_eq!(wildcard_base("static.example.com"), None);
    }

    #[test]
    fn cert_identifiers_include_the_bare_parent() {
        assert_eq!(
            cert_identifiers("*.dev.example.com"),
            vec![
                "*.dev.example.com".to_string(),
                "dev.example.com".to_string()
            ]
        );
    }

    #[test]
    fn cert_identifiers_of_a_plain_name_is_just_that_name() {
        assert_eq!(
            cert_identifiers("hoster.example.com"),
            vec!["hoster.example.com".to_string()]
        );
    }

    #[test]
    fn settings_carries_public_ip() {
        let s = Settings {
            listen: "127.0.0.1:0".into(),
            api_listen: "127.0.0.1:0".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "".into(),
            token: "t".into(),
            dashboard_password: None,
            https_listen: None,
            cert_dir: "/tmp".into(),
            public_ip: Some("1.2.3.4".into()),
            proxy_mode: ProxyMode::Standalone,
            nginx_conf_path: "/etc/nginx/conf.d/hoster.conf".into(),
            nginx_reload_cmd: "systemctl reload nginx".into(),
        };
        assert_eq!(s.public_ip.as_deref(), Some("1.2.3.4"));
    }

    #[test]
    fn settings_debug_redacts_the_token_and_dashboard_password() {
        let settings = Settings {
            listen: "127.0.0.1:8080".to_string(),
            api_listen: "127.0.0.1:8081".to_string(),
            hostname_template: "{service}-{branch}.dev.example.com".to_string(),
            registry: "localhost:5000".to_string(),
            token: "topsecret_bearer_token".to_string(),
            dashboard_password: Some("topsecret_dashboard_password".to_string()),
            https_listen: None,
            cert_dir: "/var/lib/hoster/certs".to_string(),
            public_ip: None,
            proxy_mode: ProxyMode::Standalone,
            nginx_conf_path: "/etc/nginx/conf.d/hoster.conf".into(),
            nginx_reload_cmd: "systemctl reload nginx".into(),
        };
        let dbg = format!("{settings:?}");
        assert!(
            !dbg.contains("topsecret_bearer_token"),
            "bearer token leaked via Debug: {dbg}"
        );
        assert!(
            !dbg.contains("topsecret_dashboard_password"),
            "dashboard password leaked via Debug: {dbg}"
        );
        assert!(
            dbg.contains("127.0.0.1:8080"),
            "non-secret fields should still be visible: {dbg}"
        );
    }

    #[test]
    fn proxy_mode_parses_known_values_case_insensitively() {
        assert_eq!(
            ProxyMode::parse("standalone").unwrap(),
            ProxyMode::Standalone
        );
        assert_eq!(ProxyMode::parse("nginx").unwrap(), ProxyMode::Nginx);
        assert_eq!(ProxyMode::parse("NGINX").unwrap(), ProxyMode::Nginx);
    }

    #[test]
    fn proxy_mode_rejects_unknown_value_with_a_clear_message() {
        let err = ProxyMode::parse("caddy").unwrap_err().to_string();
        assert!(
            err.contains("caddy"),
            "message should name the bad value: {err}"
        );
        assert!(
            err.contains("standalone") && err.contains("nginx"),
            "message should list valid values: {err}"
        );
    }
}
