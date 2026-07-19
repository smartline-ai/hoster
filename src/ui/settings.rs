//! The Settings page body.
use std::fmt::Write;

use crate::certs::{CertRow, CertSeverity};
use crate::secrets::{MaskedAcme, MaskedProject};
use crate::settings::{ProxyMode, Settings};
use crate::ui::components::html_escape;

pub fn settings_body(
    settings: &Settings,
    env: &[MaskedProject],
    acme: Option<&MaskedAcme>,
    certs: &[CertRow],
    nginx_status: Option<&crate::nginx::ApplyRecord>,
) -> String {
    let mut body = String::from(
        "<div class=\"page-head\"><h1>Settings</h1>\
<span class=\"page-sub\">How this server is configured. Read-only — set at startup.</span></div>\
<section class=\"panel\"><div class=\"col\"><div class=\"env-list\">",
    );
    let row = |body: &mut String, label: &str, value: &str| {
        let _ = write!(
            body,
            "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
            html_escape(label),
            html_escape(value),
        );
    };
    row(&mut body, "Hostname template", &settings.hostname_template);
    row(&mut body, "Registry", &settings.registry);
    row(&mut body, "Proxy listen", &settings.listen);
    row(&mut body, "API listen", &settings.api_listen);
    row(&mut body, "Version", env!("CARGO_PKG_VERSION"));
    body.push_str("</div></div></section>");
    render_proxy(
        &mut body,
        settings.proxy_mode,
        &settings.nginx_conf_path,
        nginx_status,
    );
    let bases = project_base_domains(settings, env);
    render_tls(
        &mut body,
        acme,
        certs,
        settings.public_ip.as_deref(),
        &bases,
    );
    body
}

/// Every base domain hoster manages a wildcard for: the default hostname
/// template's, plus each project's own override — already in `wildcard_base`'s
/// `*.<zone>` form. Mirrors [`crate::renewal::wanted_domains`] minus the ACME
/// control hostname, which is a single literal hostname rather than a
/// wildcard base and so is not one of the records this panel tells the
/// operator to create.
fn project_base_domains(settings: &Settings, env: &[MaskedProject]) -> Vec<String> {
    let mut out: Vec<String> = std::iter::once(settings.hostname_template.clone())
        .chain(env.iter().filter_map(|p| p.hostname_template.clone()))
        .filter_map(|t| crate::settings::wildcard_base(&t))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The read-only Proxy section: proxy mode, and (nginx mode) the generated
/// config path plus the last apply result. Mode is env-set, so nothing here
/// is editable — it mirrors how the DNS panel surfaces state.
fn render_proxy(
    body: &mut String,
    mode: ProxyMode,
    conf_path: &str,
    last: Option<&crate::nginx::ApplyRecord>,
) {
    body.push_str(
        "<section class=\"panel\"><div class=\"col\"><div class=\"col-label\">Proxy</div>",
    );
    let mode_str = match mode {
        ProxyMode::Standalone => "standalone",
        ProxyMode::Nginx => "nginx",
    };
    let _ = write!(
        body,
        "<div class=\"env-row\"><span class=\"k\">Mode</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
        html_escape(mode_str),
    );
    if mode == ProxyMode::Nginx {
        let _ = write!(
            body,
            "<div class=\"env-row\"><span class=\"k\">Nginx config</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
            html_escape(conf_path),
        );
        match last {
            None => body.push_str(
                "<div class=\"env-row\"><span class=\"k\">Last apply</span>\
<div class=\"env-meta\"><span class=\"tag warn\">not yet applied</span></div></div>",
            ),
            Some(r) => {
                let (class, label) = if r.validated && r.reloaded {
                    ("tag ok", "reloaded")
                } else if r.validated {
                    ("tag bad", "validated, reload failed")
                } else {
                    ("tag bad", "nginx -t failed")
                };
                let _ = write!(
                    body,
                    "<div class=\"env-row\"><span class=\"k\">Last apply</span>\
<div class=\"env-meta\"><span class=\"{class}\">{}</span></div></div>",
                    html_escape(label),
                );
                if let Some(msg) = &r.message {
                    let _ = write!(
                        body,
                        "<div class=\"env-meta\"><pre>{}</pre></div>",
                        html_escape(msg),
                    );
                }
            }
        }
    }
    body.push_str("</div></section>");
}

/// The TLS & DNS section: the ACME account, the DNS provider credential, the
/// guided DNS setup panel, a manual retry, and the per-domain certificate
/// table.
fn render_tls(
    body: &mut String,
    acme: Option<&MaskedAcme>,
    certs: &[CertRow],
    public_ip: Option<&str>,
    bases: &[String],
) {
    body.push_str(
        "<section class=\"panel\"><div class=\"col\">\
<div class=\"col-label\">TLS &amp; DNS</div>",
    );

    match acme {
        None => body.push_str(
            "<div class=\"empty\">TLS is not configured. Certificates are not being issued and \
every domain is served over plain HTTP. Add an ACME account below to start issuing \
Let's Encrypt certificates.</div>",
        ),
        Some(a) => {
            body.push_str("<div class=\"env-list\">");
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag\">ACME account</span></div></div>",
                html_escape(&a.email),
            );
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag\">control hostname</span></div></div>",
                html_escape(a.control_hostname.as_deref().unwrap_or("\u{2014} not set")),
            );
            render_dns_row(body, a);
            body.push_str("</div>");
        }
    }

    render_dns_setup(body, acme, public_ip, bases);
    render_acme_forms(body, acme);
    render_cert_table(body, certs);
    body.push_str("</div></section>");
}

/// The DNS credential row. The token itself can rewrite DNS for the whole
/// zone, so it is *never* rendered — only ever a masked placeholder, exactly
/// as project variables and registry passwords are handled.
fn render_dns_row(body: &mut String, a: &MaskedAcme) {
    let Some(kind) = a.provider_kind.as_deref() else {
        body.push_str(
            "<div class=\"env-row\"><span class=\"k\">No DNS provider</span>\
<div class=\"env-meta\"><span class=\"tag bad\">DNS-01 unavailable</span></div></div>",
        );
        return;
    };
    let _ = write!(
        body,
        "<div class=\"env-row\"><span class=\"k\">{}</span>",
        html_escape(kind),
    );
    if a.token_set {
        body.push_str(
            "<form method=\"post\" action=\"/ui/acme/dns/delete\" \
onsubmit=\"return confirm('Remove the DNS provider token?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Remove DNS token\">\u{2715}</button></form>",
        );
    }
    body.push_str("<div class=\"env-meta\">");
    if a.token_set {
        body.push_str(
            "<span class=\"val\">\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}</span>\
<span class=\"tag\">DNS token</span>",
        );
    } else {
        body.push_str("<span class=\"tag bad\">no token set</span>");
    }
    body.push_str("</div></div>");
}

/// The four DNS provider kinds the guided picker offers, in the fixed order
/// used for the `<select>` and everywhere else a stable order is needed.
const PROVIDER_KINDS: [&str; 4] = ["cloudflare", "hetzner", "namecheap", "manual"];

/// The guided DNS setup section: the resolved `HOSTER_PUBLIC_IP` (with an
/// inline warning if it is unset while a non-manual provider is configured),
/// the provider picker with every kind's fields and help — including
/// Namecheap's IP-allowlist precondition — the literal records hoster
/// expects for every project base domain, and (once a non-manual provider is
/// saved) a "check" affordance.
///
/// Rendered regardless of whether ACME is configured yet: DNS is the step
/// operators get wrong before they ever reach issuance, so the guidance and
/// the records to create are useful before the ACME account exists, not only
/// after.
fn render_dns_setup(
    body: &mut String,
    acme: Option<&MaskedAcme>,
    public_ip: Option<&str>,
    bases: &[String],
) {
    body.push_str("<div class=\"col-label\">DNS setup</div>");
    render_public_ip_status(body, acme, public_ip);
    render_provider_picker(body, acme);
    render_provider_help(body, public_ip);
    render_managed_records(body, bases, public_ip, acme.is_some());
    if let Some(a) = acme {
        render_dns_check(body, a);
    }
}

/// The resolved `HOSTER_PUBLIC_IP`, and — because DNS is the step operators
/// get wrong silently — an inline warning naming it the moment a non-manual
/// provider is configured without it: every wildcard A record this panel
/// promises, and the Namecheap allowlist step below, depend on this value
/// being right.
fn render_public_ip_status(body: &mut String, acme: Option<&MaskedAcme>, public_ip: Option<&str>) {
    let _ = write!(
        body,
        "<div class=\"env-row\"><span class=\"k\">HOSTER_PUBLIC_IP</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
        html_escape(public_ip.unwrap_or("not set")),
    );
    let non_manual = acme
        .and_then(|a| a.provider_kind.as_deref())
        .is_some_and(|k| k != "manual");
    if public_ip.is_none() && non_manual {
        body.push_str(
            "<div class=\"reason\">HOSTER_PUBLIC_IP is not set, but a DNS provider is \
configured \u{2014} wildcard A records cannot be created until it is set (and hoster \
restarted).</div>",
        );
    }
}

/// The provider `<select>` plus every kind's fields, replacing the old
/// free-text `kind` input. Every kind's field is present in the one form —
/// only the ones the selected `kind` needs are read server-side and enforced
/// by [`crate::secrets::DnsProviderConfig::validate`] — so there is no need
/// for client-side JavaScript (this dashboard has none outside the live log
/// stream) to hide the others; `render_provider_help` says which fields
/// matter for which kind.
fn render_provider_picker(body: &mut String, acme: Option<&MaskedAcme>) {
    let current = acme.and_then(|a| a.provider_kind.as_deref());
    body.push_str("<form class=\"add-var\" method=\"post\" action=\"/ui/acme/dns\"><select name=\"kind\" required>");
    for kind in PROVIDER_KINDS {
        let selected = if current == Some(kind) {
            " selected"
        } else {
            ""
        };
        let _ = write!(body, "<option value=\"{kind}\"{selected}>{kind}</option>");
    }
    body.push_str(
        "</select>\
<input name=\"token\" type=\"password\" placeholder=\"API token \u{2014} cloudflare/hetzner\" autocomplete=\"off\">\
<input name=\"api_user\" placeholder=\"api_user \u{2014} namecheap\" autocomplete=\"off\">\
<input name=\"api_key\" type=\"password\" placeholder=\"api_key \u{2014} namecheap\" autocomplete=\"off\">\
<input name=\"username\" placeholder=\"username \u{2014} namecheap\" autocomplete=\"off\">\
<button class=\"btn primary\" type=\"submit\">Save DNS provider</button></form>",
    );
}

/// Per-kind setup help, always shown together — this dashboard has no
/// client-side JavaScript to reveal only the selected kind's help, and the
/// picker above already limits which config actually gets persisted.
fn render_provider_help(body: &mut String, public_ip: Option<&str>) {
    body.push_str("<div class=\"env-list\">");
    render_kind_help(
        body,
        "cloudflare",
        "A scoped API token with Zone:DNS:Edit permission for the zone your base domain lives in.",
    );
    render_kind_help(
        body,
        "hetzner",
        "An API token generated from the Hetzner DNS console for the zone your base domain lives in.",
    );
    render_namecheap_help(body, public_ip);
    render_kind_help(
        body,
        "manual",
        "No credentials needed \u{2014} hoster does not touch DNS. Create the records below yourself.",
    );
    body.push_str("</div>");
}

fn render_kind_help(body: &mut String, kind: &str, help: &str) {
    let _ = write!(
        body,
        "<div class=\"env-row\"><span class=\"k\">{kind}</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
        html_escape(help),
    );
}

/// Namecheap needs three fields (`api_user`, `api_key`, `username`) *and* an
/// IP allowlist on the Namecheap account itself — API calls from an
/// un-allowlisted IP are rejected before hoster's credentials are even
/// checked, so this precondition is surfaced right next to the fields it
/// blocks, with the exact IP to allowlist.
fn render_namecheap_help(body: &mut String, public_ip: Option<&str>) {
    let ip_note = public_ip.unwrap_or("HOSTER_PUBLIC_IP is not set \u{2014} set it first");
    let _ = write!(
        body,
        "<div class=\"env-row\"><span class=\"k\">namecheap</span>\
<div class=\"env-meta\"><span class=\"tag\">needs api_user, api_key, username</span>\
<span class=\"tag warn\">allowlist {} in Namecheap \u{2192} API Access first</span></div></div>",
        html_escape(ip_note),
    );
}

/// The records hoster expects to exist for every project base domain: the
/// wildcard A record every provider (or a manual operator) must create,
/// plus — while TLS is configured — the `_acme-challenge` TXT record each
/// DNS-01 validation writes and cleans up around issuance. Shown regardless
/// of the selected provider `kind` so it doubles as a "what hoster manages"
/// reference to check a saved provider's DNS-01 automation against.
fn render_managed_records(
    body: &mut String,
    bases: &[String],
    public_ip: Option<&str>,
    tls_on: bool,
) {
    body.push_str("<div class=\"col-label\">Records hoster manages</div>");
    if bases.is_empty() {
        body.push_str("<div class=\"empty\">No project base domains yet.</div>");
        return;
    }
    let ip = public_ip.unwrap_or("HOSTER_PUBLIC_IP not set");
    body.push_str("<div class=\"env-list\">");
    for base in bases {
        let _ = write!(
            body,
            "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag\">A</span><span class=\"val\">{}</span></div></div>",
            html_escape(base),
            html_escape(ip),
        );
        if tls_on {
            let apex = base.strip_prefix("*.").unwrap_or(base);
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">_acme-challenge.{}</span>\
<div class=\"env-meta\"><span class=\"tag\">TXT</span>\
<span class=\"val\">set by hoster during issuance</span></div></div>",
                html_escape(apex),
            );
        }
    }
    body.push_str("</div>");
}

/// A "check DNS" affordance next to a saved non-manual provider. There is no
/// verify endpoint yet — nothing resolves the base domain and compares it
/// against `HOSTER_PUBLIC_IP` — so this is deliberately inert and labeled as
/// such rather than pretending to call somewhere real. Wire this up to a
/// resolver check instead of adding a second button once one exists.
fn render_dns_check(body: &mut String, a: &MaskedAcme) {
    let Some(kind) = a.provider_kind.as_deref() else {
        return;
    };
    if kind == "manual" {
        return;
    }
    body.push_str(
        "<div class=\"retry\">\
<button class=\"btn\" type=\"button\" disabled title=\"Automated DNS verification is not built yet\">\
Check DNS (not yet automated)</button>\
<span class=\"hint\">Automated verification isn't wired up yet \u{2014} confirm the record above \
resolves, e.g. with <code>dig +short &lt;base&gt; A</code>.</span></div>",
    );
}

fn render_acme_forms(body: &mut String, acme: Option<&MaskedAcme>) {
    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/acme/config\">\
<input name=\"email\" type=\"email\" placeholder=\"account email\" required>\
<input name=\"control_hostname\" placeholder=\"control hostname \u{2014} optional\">\
<button class=\"btn primary\" type=\"submit\">{}</button></form>",
        if acme.is_some() {
            "Replace ACME account"
        } else {
            "Set up TLS"
        },
    );
    // The retry affordance only means anything once there is a configuration
    // to retry; without it the sole recovery from bad credentials is waiting
    // up to six hours for the next scheduled renewal pass.
    if acme.is_some() {
        body.push_str(
            "<form method=\"post\" action=\"/ui/acme/renew\" class=\"retry\">\
<button class=\"btn\" type=\"submit\">Retry now</button>\
<span class=\"hint\">Runs a renewal pass immediately \u{2014} use this after fixing credentials.</span>\
</form>",
        );
    }
}

/// The per-domain certificate table. A domain whose certificate could not be
/// obtained keeps serving plain HTTP rather than going dark; that is only an
/// acceptable failure mode because it is visible here, with its reason.
fn render_cert_table(body: &mut String, certs: &[CertRow]) {
    let _ = write!(
        body,
        "<div class=\"col-label\">Certificates <span class=\"count\">{}</span></div>",
        certs.len(),
    );
    if certs.is_empty() {
        body.push_str("<div class=\"empty\">No domains need a certificate yet.</div>");
        return;
    }
    body.push_str("<div class=\"env-list\">");
    for c in certs {
        let (class, label, reason) = cert_state_parts(c);
        let _ = write!(
            body,
            "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag {class}\">{}</span>",
            html_escape(&c.domain),
            html_escape(label),
        );
        if let Some(r) = reason {
            let _ = write!(body, "<span class=\"reason\">{}</span>", html_escape(r));
        }
        body.push_str("</div></div>");
    }
    body.push_str("</div>");
}

/// Map a row's typed [`CertSeverity`] to a CSS class, a short label, and —
/// for a failure — the reason, which is surfaced beside the label rather
/// than buried inside it. Severity drives the styling directly; `state`'s
/// wording is only ever used to extract the human-readable reason text, so a
/// reworded failure message can never lose its failure styling the way
/// string-matching `state`'s prefix used to allow.
fn cert_state_parts(row: &CertRow) -> (&'static str, &str, Option<&str>) {
    match row.severity {
        CertSeverity::Failed => {
            // `state` is normally `"failed: <reason>"`; strip that prefix
            // when present, but fall back to the whole string so a reworded
            // message still surfaces as the reason rather than disappearing.
            let reason = row
                .state
                .strip_prefix("failed:")
                .map(str::trim)
                .unwrap_or(row.state.as_str());
            ("bad", "failed", Some(reason).filter(|r| !r.is_empty()))
        }
        CertSeverity::Valid => ("ok", &row.state, None),
        CertSeverity::Pending => ("warn", &row.state, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ProxyMode;

    fn settings() -> Settings {
        Settings {
            listen: "0.0.0.0:80".into(),
            api_listen: "0.0.0.0:8081".into(),
            hostname_template: "{service}-{branch}.dev.example.com".into(),
            registry: "ghcr.io".into(),
            token: "super-secret-token".into(),
            dashboard_password: Some("hunter2".into()),
            https_listen: None,
            cert_dir: "/var/lib/hoster/certs".into(),
            public_ip: None,
            proxy_mode: ProxyMode::Standalone,
            nginx_conf_path: "/etc/nginx/conf.d/hoster.conf".into(),
            nginx_reload_cmd: "systemctl reload nginx".into(),
        }
    }

    fn acme() -> MaskedAcme {
        MaskedAcme {
            email: "me@example.com".into(),
            control_hostname: Some("hoster.example.com".into()),
            provider_kind: Some("cloudflare".into()),
            token_set: true,
        }
    }

    fn row(domain: &str, state: &str, severity: CertSeverity) -> CertRow {
        CertRow {
            domain: domain.into(),
            state: state.into(),
            severity,
        }
    }

    fn project(hostname_template: Option<&str>) -> MaskedProject {
        MaskedProject {
            project: "demo".into(),
            vars: vec![],
            registry: None,
            hostname_template: hostname_template.map(str::to_string),
        }
    }

    #[test]
    fn shows_system_info_but_never_secrets() {
        let html = settings_body(&settings(), &[], None, &[], None);
        assert!(html.contains("{service}-{branch}.dev.example.com"));
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("0.0.0.0:8081"));
        // secrets must never render
        assert!(!html.contains("super-secret-token"));
        assert!(!html.contains("hunter2"));
    }

    /// Unconfigured ACME must read as "not set up yet", not as an empty panel
    /// an operator could mistake for a working configuration.
    #[test]
    fn shows_setup_prompt_when_acme_is_unconfigured() {
        let html = settings_body(&settings(), &[], None, &[], None);
        assert!(html.to_lowercase().contains("tls"));
        assert!(html.to_lowercase().contains("not configured"));
        // the form that configures it is still reachable
        assert!(html.contains("action=\"/ui/acme/config\""));
    }

    /// The DNS row names the provider and shows the token only as a masked
    /// placeholder, with forms to replace or remove it. This does not (and
    /// structurally cannot) guard against the token itself leaking: `acme()`
    /// is a [`MaskedAcme`] fixture, and that type has no field capable of
    /// holding the plaintext token in the first place — that guarantee comes
    /// from `MaskedAcme`'s shape, plus the end-to-end
    /// `dashboard_pages_never_render_the_dns_token` test in `crate::api`,
    /// which stores a real token through `set_dns_provider` and inspects the
    /// actual rendered page.
    #[test]
    fn renders_the_dns_provider_masked_and_with_manage_actions() {
        let html = settings_body(&settings(), &[], Some(&acme()), &[], None);
        // the provider is named and the token shown only as bullets
        assert!(html.contains("cloudflare"));
        assert!(html.contains('\u{2022}'));
        // and it can be replaced or removed
        assert!(html.contains("action=\"/ui/acme/dns\""));
        assert!(html.contains("action=\"/ui/acme/dns/delete\""));
    }

    /// The retry affordance, without which the only recovery from bad
    /// credentials is waiting up to six hours for the next scheduled pass.
    #[test]
    fn shows_retry_control_once_acme_is_configured() {
        let configured = settings_body(&settings(), &[], Some(&acme()), &[], None);
        assert!(configured.contains("action=\"/ui/acme/renew\""));
        assert!(configured.contains("me@example.com"));
        assert!(configured.contains("hoster.example.com"));
    }

    /// Per-domain certificate state, including a failure that must show its
    /// reason: that domain is serving plain HTTP until the operator fixes it.
    #[test]
    fn renders_certificate_state_rows_including_a_failure_reason() {
        let rows = [
            row(
                "a.example.com",
                "valid until 2026-10-01",
                CertSeverity::Valid,
            ),
            row("b.example.com", "pending", CertSeverity::Pending),
            row(
                "c.example.com",
                "failed: no zone found",
                CertSeverity::Failed,
            ),
        ];
        let html = settings_body(&settings(), &[], Some(&acme()), &rows, None);
        assert!(html.contains("a.example.com"));
        assert!(html.contains("valid until 2026-10-01"));
        assert!(html.contains("b.example.com"));
        assert!(html.contains("pending"));
        assert!(html.contains("c.example.com"));
        // the failure reason itself is legible, not just the word "failed"
        assert!(html.contains("no zone found"));
    }

    /// Certificate state is free-form text built from provider errors.
    #[test]
    fn escapes_certificate_state() {
        let rows = [row(
            "<b>d</b>.example.com",
            "failed: <script>alert(1)</script>",
            CertSeverity::Failed,
        )];
        let html = settings_body(&settings(), &[], Some(&acme()), &rows, None);
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<b>d</b>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    /// Severity is set structurally by the caller, not re-derived from
    /// `state`'s wording — so a reworded failure message that no longer
    /// starts with "failed:" must still render with the failure (`bad`)
    /// styling and its reason. Before the fix, `cert_state_parts` string-
    /// matched `state`'s prefix, so this exact row rendered as a neutral
    /// "warn" row instead of a red one.
    #[test]
    fn a_failed_row_keeps_failure_styling_even_when_its_wording_no_longer_says_failed() {
        let rows = [row(
            "e.example.com",
            "DNS challenge could not be completed",
            CertSeverity::Failed,
        )];
        let html = settings_body(&settings(), &[], Some(&acme()), &rows, None);
        assert!(
            html.contains("tag bad"),
            "a Failed row must render with the bad/red styling regardless of wording: {html}"
        );
        assert!(
            html.contains("DNS challenge could not be completed"),
            "the failure reason must still be visible: {html}"
        );
    }

    /// The guided picker must offer every provider kind, and the resolved
    /// `HOSTER_PUBLIC_IP` must be visible even before a provider is saved
    /// (`provider_kind: None` here) — operators need to see it is already
    /// right before they ever touch the DNS form.
    #[test]
    fn dns_panel_lists_all_four_providers_and_shows_public_ip() {
        let mut s = settings();
        s.public_ip = Some("1.2.3.4".to_string());
        let mut unconfigured = acme();
        unconfigured.provider_kind = None;
        unconfigured.token_set = false;
        let env = [project(None)];
        let html = settings_body(&s, &env, Some(&unconfigured), &[], None);
        for kind in PROVIDER_KINDS {
            assert!(
                html.contains(&format!("value=\"{kind}\"")),
                "picker must offer {kind}: {html}"
            );
        }
        assert!(html.contains("1.2.3.4"), "must surface HOSTER_PUBLIC_IP");
    }

    /// The literal wildcard A record hoster expects for a project base
    /// domain, for copy-paste into a manual zone (or to check a configured
    /// provider's automation against) — this is the "records hoster manages"
    /// summary `render_managed_records` renders below the picker.
    #[test]
    fn dns_panel_manual_mode_shows_the_record_to_create() {
        let mut html = String::new();
        render_managed_records(
            &mut html,
            &["*.dev.example.com".to_string()],
            Some("1.2.3.4"),
            false,
        );
        assert!(html.contains("*.dev.example.com"));
        assert!(html.contains("A"));
        assert!(html.contains("1.2.3.4"));
        // TLS is off in this deploy, so no ACME TXT note is owed.
        assert!(!html.contains("_acme-challenge"));
    }

    /// While TLS is configured, the manual-records summary also names the
    /// `_acme-challenge` TXT record DNS-01 issuance depends on, so an
    /// operator on `manual` mode knows to create that one too.
    #[test]
    fn dns_panel_manual_records_include_the_acme_challenge_txt_note_when_tls_is_on() {
        let mut html = String::new();
        render_managed_records(
            &mut html,
            &["*.dev.example.com".to_string()],
            Some("1.2.3.4"),
            true,
        );
        assert!(html.contains("_acme-challenge.dev.example.com"));
        assert!(html.contains("TXT"));
    }

    /// A non-manual provider with `HOSTER_PUBLIC_IP` unset must produce an
    /// explicit warning naming the variable — silently skipping the wildcard
    /// A record is exactly the failure mode this panel exists to prevent.
    /// The warning must be conditional: present for a configured non-manual
    /// provider with no IP, and absent both once the IP is set and when the
    /// provider is `manual` (which never needs one).
    #[test]
    fn dns_panel_flags_missing_public_ip_for_non_manual() {
        const WARNING: &str = "wildcard A records cannot be created";
        let mut no_ip = settings();
        no_ip.public_ip = None;
        let cloudflare = acme(); // provider_kind: Some("cloudflare")

        let html = settings_body(&no_ip, &[], Some(&cloudflare), &[], None);
        assert!(html.to_lowercase().contains("hoster_public_ip"));
        assert!(html.contains(WARNING), "must warn the IP is unset: {html}");

        let mut with_ip = settings();
        with_ip.public_ip = Some("9.9.9.9".to_string());
        let html_ok = settings_body(&with_ip, &[], Some(&cloudflare), &[], None);
        assert!(
            !html_ok.contains(WARNING),
            "no warning once the IP is set: {html_ok}"
        );

        let mut manual = acme();
        manual.provider_kind = Some("manual".to_string());
        let html_manual = settings_body(&no_ip, &[], Some(&manual), &[], None);
        assert!(
            !html_manual.contains(WARNING),
            "manual mode never needs HOSTER_PUBLIC_IP: {html_manual}"
        );
    }

    /// Namecheap's API rejects calls from an un-allowlisted IP before it
    /// even looks at the credentials, so the help text must both name the
    /// precondition and show the exact IP the operator has to allowlist.
    #[test]
    fn dns_panel_shows_namecheap_allowlist_precondition() {
        let mut html = String::new();
        render_namecheap_help(&mut html, Some("1.2.3.4"));
        assert!(html.to_lowercase().contains("allowlist"));
        assert!(html.contains("1.2.3.4"), "show the IP to allowlist: {html}");
    }

    /// A saved non-manual provider gets a "check" affordance; `manual` (no
    /// automation to verify) and an unconfigured provider get none.
    #[test]
    fn dns_panel_shows_a_check_affordance_only_for_a_saved_non_manual_provider() {
        let html = settings_body(&settings(), &[], Some(&acme()), &[], None);
        assert!(
            html.contains("Check DNS"),
            "expected a check affordance: {html}"
        );

        let mut manual = acme();
        manual.provider_kind = Some("manual".to_string());
        let html_manual = settings_body(&settings(), &[], Some(&manual), &[], None);
        assert!(!html_manual.contains("Check DNS"));

        let mut none_yet = acme();
        none_yet.provider_kind = None;
        let html_none = settings_body(&settings(), &[], Some(&none_yet), &[], None);
        assert!(!html_none.contains("Check DNS"));
    }

    /// `HOSTER_PUBLIC_IP` is read straight off the environment with no
    /// validation beyond "non-empty" (see `main.rs`), so it is exactly the
    /// kind of operator-controlled value the escaping discipline exists for.
    /// It is rendered in three places on this panel — the status row, the
    /// managed-records value, and the Namecheap allowlist note — and all
    /// three must escape it exactly once.
    #[test]
    fn escapes_a_malicious_public_ip() {
        let mut s = settings();
        s.public_ip = Some("<script>alert(1)</script>".to_string());
        let env = [project(None)];
        let html = settings_body(&s, &env, Some(&acme()), &[], None);
        assert!(
            !html.contains("<script>alert(1)"),
            "public IP must be escaped: {html}"
        );
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    /// A project's hostname template is operator input too (set via the
    /// project/domain form or API, not something hoster generates) and flows
    /// into the base-domain list this panel renders unescaped by
    /// `wildcard_base` itself — so, same as `escapes_certificate_state`
    /// above, the render site must escape it regardless of what today's
    /// `validate_hostname_template` currently permits upstream.
    #[test]
    fn escapes_a_malicious_project_base_domain() {
        let env = [project(Some(
            "{service}.<script>alert(1)</script>.example.com",
        ))];
        let html = settings_body(&settings(), &env, Some(&acme()), &[], None);
        assert!(
            !html.contains("<script>alert(1)"),
            "project base domain must be escaped: {html}"
        );
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    /// The Proxy section in nginx mode names the mode, the generated conf
    /// path, and shows a success indicator once the last apply both
    /// validated and reloaded cleanly.
    #[test]
    fn proxy_section_shows_mode_and_last_apply() {
        use crate::nginx::ApplyRecord;
        let mut body = String::new();
        render_proxy(
            &mut body,
            ProxyMode::Nginx,
            "/etc/nginx/conf.d/hoster.conf",
            Some(&ApplyRecord {
                validated: true,
                reloaded: true,
                message: None,
                at: 0,
            }),
        );
        assert!(body.contains("nginx"), "{body}");
        assert!(body.contains("/etc/nginx/conf.d/hoster.conf"), "{body}");
        assert!(body.contains("reloaded"), "{body}");
    }

    /// In standalone mode the nginx conf path and last-apply status are not
    /// hoster's concern — nothing here is editable, and there is nothing to
    /// report, so those details must not appear.
    #[test]
    fn proxy_section_standalone_hides_nginx_details() {
        let mut body = String::new();
        render_proxy(
            &mut body,
            ProxyMode::Standalone,
            "/etc/nginx/conf.d/hoster.conf",
            None,
        );
        assert!(body.contains("standalone"), "{body}");
        assert!(
            !body.contains("hoster.conf"),
            "no nginx path in standalone: {body}"
        );
    }
}
