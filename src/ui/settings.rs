//! The Settings page body.
use std::fmt::Write;

use crate::certs::CertRow;
use crate::secrets::MaskedAcme;
use crate::settings::Settings;
use crate::ui::components::html_escape;

pub fn settings_body(settings: &Settings, acme: Option<&MaskedAcme>, certs: &[CertRow]) -> String {
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
    render_tls(&mut body, acme, certs);
    body
}

/// The TLS & DNS section: the ACME account, the DNS provider credential, a
/// manual retry, and the per-domain certificate table.
fn render_tls(body: &mut String, acme: Option<&MaskedAcme>, certs: &[CertRow]) {
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
    body.push_str(
        "<form class=\"add-var\" method=\"post\" action=\"/ui/acme/dns\">\
<input name=\"kind\" placeholder=\"cloudflare\" required>\
<input name=\"token\" type=\"password\" placeholder=\"API token\" autocomplete=\"off\" required>\
<button class=\"btn primary\" type=\"submit\">Save DNS token</button></form>",
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
        let (class, label, reason) = cert_state_parts(&c.state);
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

/// Split a free-form certificate state into a severity class, a short label,
/// and — for a failure — the reason, which is surfaced beside the label rather
/// than buried inside it.
fn cert_state_parts(state: &str) -> (&'static str, &str, Option<&str>) {
    if let Some(reason) = state.strip_prefix("failed:") {
        let reason = reason.trim();
        return ("bad", "failed", Some(reason).filter(|r| !r.is_empty()));
    }
    if state.starts_with("valid") {
        ("ok", state, None)
    } else {
        ("warn", state, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn row(domain: &str, state: &str) -> CertRow {
        CertRow {
            domain: domain.into(),
            state: state.into(),
        }
    }

    #[test]
    fn shows_system_info_but_never_secrets() {
        let html = settings_body(&settings(), None, &[]);
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
        let html = settings_body(&settings(), None, &[]);
        assert!(html.to_lowercase().contains("tls"));
        assert!(html.to_lowercase().contains("not configured"));
        // the form that configures it is still reachable
        assert!(html.contains("action=\"/ui/acme/config\""));
    }

    /// The DNS token can rewrite DNS. Only a masked placeholder may render.
    #[test]
    fn never_renders_the_dns_token() {
        let html = settings_body(&settings(), Some(&acme()), &[]);
        assert!(!html.contains("cf_topsecret"));
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
        let configured = settings_body(&settings(), Some(&acme()), &[]);
        assert!(configured.contains("action=\"/ui/acme/renew\""));
        assert!(configured.contains("me@example.com"));
        assert!(configured.contains("hoster.example.com"));
    }

    /// Per-domain certificate state, including a failure that must show its
    /// reason: that domain is serving plain HTTP until the operator fixes it.
    #[test]
    fn renders_certificate_state_rows_including_a_failure_reason() {
        let rows = [
            row("a.example.com", "valid until 2026-10-01"),
            row("b.example.com", "pending"),
            row("c.example.com", "failed: no zone found"),
        ];
        let html = settings_body(&settings(), Some(&acme()), &rows);
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
        )];
        let html = settings_body(&settings(), Some(&acme()), &rows);
        assert!(!html.contains("<script>alert(1)"));
        assert!(!html.contains("<b>d</b>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
