use std::fmt::Write;

use crate::engine::DeploymentInfo;

/// Escape the five HTML-significant characters. Applied to every dynamic value
/// rendered into a page — statuses in particular carry arbitrary error text.
pub fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

const STYLE: &str = "\
body{font-family:system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem;color:#1a1a1a}\
h1{font-size:1.4rem}table{width:100%;border-collapse:collapse;margin-top:1rem}\
th,td{text-align:left;padding:.5rem .6rem;border-bottom:1px solid #e2e2e2;vertical-align:top}\
th{font-size:.8rem;text-transform:uppercase;color:#666}\
.status{font-weight:600}.status.running{color:#137333}.status.failed{color:#c5221f}.status.provisioning{color:#b06000}\
a{color:#1558d6}button{cursor:pointer}\
.destroy{background:#c5221f;color:#fff;border:0;padding:.35rem .7rem;border-radius:4px}\
form.login{display:flex;gap:.5rem;margin-top:1rem}input{padding:.5rem;border:1px solid #ccc;border-radius:4px}\
.err{color:#c5221f;margin-top:.5rem}.empty{color:#666;margin-top:1rem}\
@media(prefers-color-scheme:dark){body{background:#111;color:#eee}th{color:#aaa}td,th{border-color:#333}}";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        html_escape(title)
    )
}

/// The login form. `error` renders a message above the form when a prior
/// attempt failed.
pub fn login_page(error: Option<&str>) -> String {
    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let body = format!(
        "<h1>hoster</h1>{err}\
<form class=\"login\" method=\"post\" action=\"/login\">\
<input type=\"password\" name=\"password\" placeholder=\"Password\" autofocus>\
<button type=\"submit\">Sign in</button></form>"
    );
    page("hoster — sign in", &body)
}

/// The deployment list, one row per branch, each with its URLs and a destroy
/// button.
pub fn dashboard_page(deployments: &[DeploymentInfo]) -> String {
    let mut body = String::new();
    let _ = write!(
        body,
        "<h1>hoster</h1><form method=\"post\" action=\"/logout\" style=\"float:right\">\
<button type=\"submit\">Sign out</button></form>"
    );

    if deployments.is_empty() {
        body.push_str("<p class=\"empty\">No deployments yet.</p>");
        return page("hoster — dashboard", &body);
    }

    body.push_str("<table><thead><tr><th>Branch</th><th>Status</th><th>URLs</th><th></th></tr></thead><tbody>");
    for d in deployments {
        let branch = html_escape(&d.branch);
        let status_class = d.status.split(':').next().unwrap_or("").trim();
        let links = if d.urls.is_empty() {
            "<span class=\"empty\">—</span>".to_string()
        } else {
            d.urls
                .values()
                .map(|u| {
                    let e = html_escape(u);
                    format!("<a href=\"{e}\">{e}</a>")
                })
                .collect::<Vec<_>>()
                .join("<br>")
        };
        let _ = write!(
            body,
            "<tr><td>{branch}</td>\
<td class=\"status {status_class}\">{}</td>\
<td>{links}</td>\
<td><form method=\"post\" action=\"/ui/destroy/{branch}\" \
onsubmit=\"return confirm('Destroy this branch?')\">\
<button class=\"destroy\" type=\"submit\">Destroy</button></form></td></tr>",
            html_escape(&d.status),
        );
    }
    body.push_str("</tbody></table>");
    page("hoster — dashboard", &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn dep(branch: &str, status: &str, urls: &[(&str, &str)]) -> DeploymentInfo {
        DeploymentInfo {
            branch: branch.to_string(),
            status: status.to_string(),
            urls: urls
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            html_escape("<script>&\"'"),
            "&lt;script&gt;&amp;&quot;&#39;"
        );
    }

    #[test]
    fn login_page_has_password_form() {
        let html = login_page(None);
        assert!(html.contains("<form"));
        assert!(html.contains("method=\"post\""));
        assert!(html.contains("action=\"/login\""));
        assert!(html.contains("type=\"password\""));
        assert!(!html.contains("Invalid"));
    }

    #[test]
    fn login_page_shows_error() {
        let html = login_page(Some("Invalid password"));
        assert!(html.contains("Invalid password"));
    }

    #[test]
    fn dashboard_lists_a_deployment() {
        let html = dashboard_page(&[dep(
            "feature-x",
            "running",
            &[("backend", "http://backend-feature-x.example.com")],
        )]);
        assert!(html.contains("feature-x"));
        assert!(html.contains("running"));
        assert!(html.contains("http://backend-feature-x.example.com"));
        // a destroy form pointing at the branch
        assert!(html.contains("/ui/destroy/feature-x"));
    }

    #[test]
    fn dashboard_escapes_status_text() {
        // A failed status carrying an injection payload must be escaped.
        let html = dashboard_page(&[dep("b", "failed: <script>alert(1)</script>", &[])]);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn dashboard_empty_state() {
        let html = dashboard_page(&[]);
        assert!(html.to_lowercase().contains("no deployments"));
    }
}
