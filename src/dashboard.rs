use std::collections::BTreeSet;
use std::fmt::Write;

use crate::engine::DeploymentView;
use crate::secrets::MaskedProject;

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
.project{border:1px solid #e2e2e2;border-radius:8px;padding:1rem 1.2rem;margin-top:1.5rem}\
.project h2{font-size:1.1rem;margin:0 0 .5rem}.project h3{font-size:.8rem;text-transform:uppercase;color:#666;margin:1rem 0 .3rem}\
.muted{color:#888}code{font-family:ui-monospace,monospace;font-size:.85em}\
form.addvar{display:flex;flex-wrap:wrap;gap:.4rem;margin-top:.5rem}form.addvar input{flex:1;min-width:8rem}\
.deploy{border-top:1px solid #eee;padding:.6rem 0}.deployhead{display:flex;align-items:center;gap:.6rem}\
.branch{font-weight:600}.urls{margin:.3rem 0;font-size:.9rem}.svc{margin:.3rem 0}\
.envlist{margin:.2rem 0 .2rem 1rem;padding:0;list-style:none}details summary{cursor:pointer;color:#1558d6;font-size:.85rem}\
@media(prefers-color-scheme:dark){body{background:#111;color:#eee}th{color:#aaa}td,th{border-color:#333}\
.project{border-color:#333}.deploy{border-color:#222}}";

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

/// The dashboard: deployments and hoster-managed environment, grouped by
/// project. `env` carries only masked variables (keys + target services) —
/// values are never passed in, so they cannot be rendered.
pub fn dashboard_page(deployments: &[DeploymentView], env: &[MaskedProject]) -> String {
    let mut body = String::new();
    body.push_str(
        "<form method=\"post\" action=\"/logout\" style=\"float:right\">\
<button type=\"submit\">Sign out</button></form><h1>hoster</h1>",
    );

    // Union of every project that has a deployment or stored environment.
    let mut projects: BTreeSet<&str> = BTreeSet::new();
    for d in deployments {
        projects.insert(d.project.as_str());
    }
    for p in env {
        projects.insert(p.project.as_str());
    }

    if projects.is_empty() {
        body.push_str("<p class=\"empty\">No projects yet.</p>");
        return page("hoster — dashboard", &body);
    }

    for project in projects {
        let _ = write!(
            body,
            "<section class=\"project\"><h2>{}</h2>",
            html_escape(project)
        );
        render_environment(&mut body, project, env);
        render_deployments(&mut body, project, deployments);
        body.push_str("</section>");
    }
    page("hoster — dashboard", &body)
}

/// The environment block for one project: its masked vars (with delete forms)
/// and a form to add another.
fn render_environment(body: &mut String, project: &str, env: &[MaskedProject]) {
    body.push_str("<h3>Environment</h3>");
    let vars = env
        .iter()
        .find(|p| p.project == project)
        .map(|p| &p.vars[..])
        .unwrap_or(&[]);
    if vars.is_empty() {
        body.push_str("<p class=\"empty\">No stored variables.</p>");
    } else {
        body.push_str("<table><thead><tr><th>Key</th><th>Value</th><th>Services</th><th></th></tr></thead><tbody>");
        for v in vars {
            let key = html_escape(&v.key);
            let services = if v.services.is_empty() {
                "<span class=\"muted\">all</span>".to_string()
            } else {
                html_escape(&v.services.join(", "))
            };
            let _ = write!(
                body,
                "<tr><td><code>{key}</code></td><td class=\"muted\">••••••</td><td>{services}</td>\
<td><form method=\"post\" action=\"/ui/projects/{proj}/vars/{key}/delete\" \
onsubmit=\"return confirm('Delete this variable?')\">\
<button class=\"destroy\" type=\"submit\">Delete</button></form></td></tr>",
                proj = html_escape(project),
            );
        }
        body.push_str("</tbody></table>");
    }
    let _ = write!(
        body,
        "<form class=\"addvar\" method=\"post\" action=\"/ui/projects/{proj}/vars\">\
<input name=\"key\" placeholder=\"KEY\" required>\
<input name=\"value\" type=\"password\" placeholder=\"value\" required>\
<input name=\"services\" placeholder=\"services (comma-sep, blank = all)\">\
<button type=\"submit\">Save variable</button></form>",
        proj = html_escape(project),
    );
}

/// The deployments for one project: each branch's status, URLs, and an
/// expandable view of the config it was deployed from.
fn render_deployments(body: &mut String, project: &str, deployments: &[DeploymentView]) {
    let deps: Vec<&DeploymentView> = deployments
        .iter()
        .filter(|d| d.project == project)
        .collect();
    body.push_str("<h3>Deployments</h3>");
    if deps.is_empty() {
        body.push_str("<p class=\"empty\">No deployments.</p>");
        return;
    }
    for d in deps {
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
                .join(" · ")
        };
        let _ = write!(
            body,
            "<div class=\"deploy\"><div class=\"deployhead\">\
<span class=\"branch\">{branch}</span> \
<span class=\"status {status_class}\">{status}</span>\
<form method=\"post\" action=\"/ui/destroy/{branch}\" style=\"float:right\" \
onsubmit=\"return confirm('Destroy this branch?')\">\
<button class=\"destroy\" type=\"submit\">Destroy</button></form></div>\
<div class=\"urls\">{links}</div>",
            status = html_escape(&d.status),
        );
        render_config(body, d);
        body.push_str("</div>");
    }
}

/// The `<details>` config view for one deployment: per service the image,
/// exposed port, and its `hoster.json` env (shown in plaintext).
fn render_config(body: &mut String, d: &DeploymentView) {
    let Some(cfg) = &d.config else {
        body.push_str("<p class=\"muted\">config unavailable</p>");
        return;
    };
    body.push_str("<details><summary>config</summary>");
    for (name, svc) in &cfg.services {
        let _ = write!(
            body,
            "<div class=\"svc\"><strong>{}</strong> <code>{}</code>",
            html_escape(name),
            html_escape(&svc.image),
        );
        if let Some(exp) = &svc.expose {
            let _ = write!(body, " <span class=\"muted\">:{} exposed</span>", exp.port);
        }
        if svc.env.is_empty() {
            body.push_str("</div>");
            continue;
        }
        body.push_str("<ul class=\"envlist\">");
        for (k, v) in &svc.env {
            let _ = write!(
                body,
                "<li><code>{}={}</code></li>",
                html_escape(k),
                html_escape(v),
            );
        }
        body.push_str("</ul></div>");
    }
    body.push_str("</details>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::secrets::MaskedVar;
    use std::collections::BTreeMap;

    fn view(project: &str, branch: &str, status: &str, config_json: &str) -> DeploymentView {
        let config = config::parse(config_json).ok();
        let urls = BTreeMap::new();
        DeploymentView {
            project: project.to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            urls,
            config,
        }
    }

    fn masked(project: &str, vars: &[(&str, &[&str])]) -> MaskedProject {
        MaskedProject {
            project: project.to_string(),
            vars: vars
                .iter()
                .map(|(k, svcs)| MaskedVar {
                    key: k.to_string(),
                    services: svcs.iter().map(|s| s.to_string()).collect(),
                })
                .collect(),
        }
    }

    const CFG: &str = r#"{"project":"odinvestor","services":{
        "backend":{"image":"reg/backend:abc","env":{"PORT":"8080"},"expose":{"port":8080}}
    }}"#;

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
        assert!(html.contains("action=\"/login\""));
        assert!(html.contains("type=\"password\""));
    }

    #[test]
    fn login_page_shows_error() {
        assert!(login_page(Some("Invalid password")).contains("Invalid password"));
    }

    #[test]
    fn groups_deployments_and_env_under_their_project() {
        let html = dashboard_page(
            &[view("odinvestor", "b1", "running", CFG)],
            &[masked("odinvestor", &[("GOOGLE_API_KEY", &["backend"])])],
        );
        assert!(html.contains("odinvestor"));
        assert!(html.contains("b1"));
        assert!(html.contains("running"));
        // env var key + target service surface; a destroy form points at the branch
        assert!(html.contains("GOOGLE_API_KEY"));
        assert!(html.contains("backend"));
        assert!(html.contains("/ui/destroy/b1"));
    }

    #[test]
    fn shows_service_image_and_env_from_config() {
        let html = dashboard_page(&[view("odinvestor", "b1", "running", CFG)], &[]);
        assert!(html.contains("reg/backend:abc"));
        assert!(html.contains("PORT=8080"));
    }

    #[test]
    fn masked_var_shows_key_not_value_and_has_forms() {
        let html = dashboard_page(&[], &[masked("p", &[("SECRET", &[])])]);
        assert!(html.contains("SECRET"));
        // add-var and delete forms target the project routes
        assert!(html.contains("action=\"/ui/projects/p/vars\""));
        assert!(html.contains("/ui/projects/p/vars/SECRET/delete"));
        // no plaintext value is present for a masked var
        assert!(html.contains("••••") || html.contains("&bull;") || html.contains("•"));
    }

    #[test]
    fn project_with_only_env_still_renders() {
        let html = dashboard_page(&[], &[masked("secretsonly", &[("K", &[])])]);
        assert!(html.contains("secretsonly"));
        assert!(html.contains("No deployments"));
    }

    #[test]
    fn dashboard_escapes_status_text() {
        let html = dashboard_page(
            &[view("p", "b", "failed: <script>alert(1)</script>", CFG)],
            &[],
        );
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn dashboard_empty_state() {
        let html = dashboard_page(&[], &[]);
        assert!(html.to_lowercase().contains("no projects"));
    }
}
