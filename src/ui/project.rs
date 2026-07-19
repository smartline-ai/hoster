//! The Project page body — filled in Task 6.
use std::fmt::Write;

use crate::engine::DeploymentView;
use crate::secrets::MaskedProject;
use crate::ui::components::{EXT_ICON, html_escape, plural};
use crate::ui::overview::status_word;

pub fn project_body(project: &str, deps: &[&DeploymentView], env: &[MaskedProject]) -> String {
    let vars = env
        .iter()
        .find(|p| p.project == project)
        .map(|p| p.vars.len())
        .unwrap_or(0);
    let running = deps
        .iter()
        .filter(|d| status_word(&d.status) == "running")
        .count();
    let esc = html_escape(project);

    let mut body = format!(
        "<div class=\"page-head\"><h1>{esc}</h1><span class=\"page-sub\">{} · {} · {}</span></div>",
        plural(deps.len(), "branch", "branches"),
        plural(running, "running", "running"),
        plural(vars, "variable", "variables"),
    );

    body.push_str(
        "<section class=\"panel\"><div class=\"panel-body\" style=\"grid-template-columns:1fr\">",
    );
    render_deployments(&mut body, project, deps);
    render_environment(&mut body, project, env);
    render_registry(&mut body, project, env);
    body.push_str("</div></section>");

    body.push_str(LOG_SCRIPT);
    body
}

fn render_deployments(body: &mut String, project: &str, deps: &[&DeploymentView]) {
    let _ = write!(
        body,
        "<div class=\"col\"><div class=\"col-label\">Deployments <span class=\"count\">{}</span></div>",
        deps.len()
    );
    if deps.is_empty() {
        body.push_str("<div class=\"empty\">No deployments yet.</div></div>");
        return;
    }
    let proj = html_escape(project);
    for d in deps {
        let branch = html_escape(&d.branch);
        let (word, reason) = match d.status.split_once(':') {
            Some((w, r)) => (w.trim(), Some(r.trim()).filter(|r| !r.is_empty())),
            None => (d.status.trim(), None),
        };
        let word_e = html_escape(word);
        let _ = write!(
            body,
            "<article class=\"deploy is-{word_e}\"><span class=\"led\"></span><div class=\"deploy-main\">\
<div class=\"deploy-row1\"><span class=\"branch\">{branch}</span>\
<span class=\"pill {word_e}\"><span class=\"dot\"></span>{word_e}</span></div>",
        );
        if word == "failed" {
            if let Some(r) = reason {
                let _ = write!(body, "<div class=\"reason\">{}</div>", html_escape(r));
            }
        } else if !d.urls.is_empty() {
            body.push_str("<div class=\"urls\">");
            for u in d.urls.values() {
                let e = html_escape(u);
                let _ = write!(
                    body,
                    "<a class=\"chip\" href=\"{e}\"><span class=\"host\">{e}</span>{EXT_ICON}</a>"
                );
            }
            body.push_str("</div>");
        }
        render_config_and_logs(body, &proj, &branch, d);
        let _ = write!(
            body,
            "</div><form method=\"post\" action=\"/ui/destroy/{branch}\" \
onsubmit=\"return confirm('Destroy this branch?')\">\
<button class=\"btn danger\" type=\"submit\" title=\"Destroy branch\">Destroy</button></form></article>",
        );
    }
    body.push_str("</div>");
}

/// The per-service config block, each service carrying a live-log toggle. The
/// branch here is already HTML-escaped by the caller.
fn render_config_and_logs(body: &mut String, project: &str, branch: &str, d: &DeploymentView) {
    let Some(cfg) = &d.config else {
        body.push_str(
            "<p class=\"reason\" style=\"color:var(--faint)\">configuration unavailable</p>",
        );
        return;
    };
    body.push_str("<div class=\"svc-grid\">");
    for (name, svc) in &cfg.services {
        let svc_name = html_escape(name);
        let _ = write!(
            body,
            "<div class=\"svc\"><div class=\"svc-head\"><span class=\"svc-name\">{svc_name}</span>",
        );
        if let Some(exp) = &svc.expose {
            let _ = write!(body, "<span class=\"port\">:{}</span>", exp.port);
        }
        let _ = write!(
            body,
            "</div><code class=\"img\">{}</code>",
            html_escape(&svc.image)
        );
        if !svc.env.is_empty() {
            body.push_str("<ul class=\"env-inline\">");
            for (k, v) in &svc.env {
                let _ = write!(
                    body,
                    "<li><span class=\"k\">{}</span><span class=\"eq\">=</span>{}</li>",
                    html_escape(k),
                    html_escape(v),
                );
            }
            body.push_str("</ul>");
        }
        // Live log panel: data-url is read by LOG_SCRIPT to open an EventSource
        // on expand and close it on collapse. project & branch are pre-escaped;
        // the service name path segment uses the raw name url-safe enough for
        // our service naming (alnum/dash) — escape it for the attribute too.
        let _ = write!(
            body,
            "<details class=\"logs\" data-url=\"/p/{project}/logs/{branch}/{svc_name}\">\
<summary><span class=\"chev\">\u{203a}</span> live logs</summary>\
<div class=\"logterm\"><span class=\"ph\">Connecting…</span></div></details>",
        );
        body.push_str("</div>");
    }
    body.push_str("</div>");
}

fn render_environment(body: &mut String, project: &str, env: &[MaskedProject]) {
    let vars = env
        .iter()
        .find(|p| p.project == project)
        .map(|p| &p.vars[..])
        .unwrap_or(&[]);
    let proj = html_escape(project);
    let _ = write!(
        body,
        "<aside class=\"col environment\"><div class=\"col-label\">Environment <span class=\"count\">{}</span></div>",
        vars.len()
    );
    if vars.is_empty() {
        body.push_str(
            "<div class=\"empty\">No variables yet. Add one below and it's injected into every deploy of this project.</div>",
        );
    } else {
        body.push_str("<div class=\"env-list\">");
        for v in vars {
            let key = html_escape(&v.key);
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{key}</span>\
<form method=\"post\" action=\"/ui/projects/{proj}/vars/{key}/delete\" \
onsubmit=\"return confirm('Delete this variable?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Delete variable\">\u{2715}</button></form>\
<div class=\"env-meta\"><span class=\"val\">\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}</span>",
            );
            if v.services.is_empty() {
                body.push_str("<span class=\"tag all\">all services</span>");
            } else {
                for s in &v.services {
                    let _ = write!(body, "<span class=\"tag\">{}</span>", html_escape(s));
                }
            }
            body.push_str("</div></div>");
        }
        body.push_str("</div>");
    }
    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/projects/{proj}/vars\">\
<input name=\"key\" placeholder=\"NEW_KEY\" required>\
<input name=\"value\" type=\"password\" placeholder=\"value\" autocomplete=\"off\" required>\
<input name=\"services\" placeholder=\"services \u{2014} comma-separated, blank = all\">\
<button class=\"btn primary\" type=\"submit\">Add variable</button></form></aside>",
    );
}

fn render_registry(body: &mut String, project: &str, env: &[MaskedProject]) {
    let cred = env
        .iter()
        .find(|p| p.project == project)
        .and_then(|p| p.registry.as_ref());
    let proj = html_escape(project);
    body.push_str(
        "<aside class=\"col registry\"><div class=\"col-label\">Registry credential</div>",
    );
    match cred {
        None => {
            body.push_str("<div class=\"empty\">No registry credential. Public images only.</div>")
        }
        Some(c) => {
            let _ = write!(
                body,
                "<div class=\"env-list\"><div class=\"env-row\"><span class=\"k\">{registry}</span>\
<form method=\"post\" action=\"/ui/projects/{proj}/registry/delete\" \
onsubmit=\"return confirm('Remove this registry credential?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Remove registry credential\">\u{2715}</button></form>\
<div class=\"env-meta\"><span class=\"val\">\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}</span>\
<span class=\"tag\">{username}</span></div></div></div>",
                registry = html_escape(&c.registry),
                username = html_escape(&c.username),
            );
        }
    }
    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/projects/{proj}/registry\">\
<input name=\"registry\" placeholder=\"ghcr.io\" required>\
<input name=\"username\" placeholder=\"username\" required>\
<input name=\"password\" type=\"password\" placeholder=\"token or password\" required>\
<button class=\"btn primary\" type=\"submit\">Save credential</button></form></aside>",
    );
}

/// Scoped client script: on expanding a `.logs` panel, open an EventSource to
/// its data-url and append lines; on collapse, close it. The only JS in the app.
const LOG_SCRIPT: &str = r#"<script>
document.querySelectorAll('details.logs').forEach(function(d){
  var term=d.querySelector('.logterm'), es=null;
  d.addEventListener('toggle',function(){
    if(d.open){
      term.textContent='';
      es=new EventSource(d.dataset.url);
      es.onmessage=function(e){
        var atBottom=term.scrollHeight-term.scrollTop-term.clientHeight<20;
        term.textContent+=e.data+'\n';
        if(atBottom)term.scrollTop=term.scrollHeight;
      };
      es.onerror=function(){ if(es){es.close();es=null;} };
    } else if(es){ es.close(); es=null; }
  });
});
</script>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::secrets::{MaskedRegistry, MaskedVar};
    use std::collections::BTreeMap;

    const CFG: &str = r#"{"project":"odin","services":{
        "backend":{"image":"reg/backend:abc","env":{"PORT":"8080"},"expose":{"port":8080}}
    }}"#;

    fn view(branch: &str, status: &str) -> DeploymentView {
        DeploymentView {
            project: "odin".to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            urls: BTreeMap::new(),
            config: config::parse(CFG).ok(),
        }
    }

    fn masked(vars: &[(&str, &[&str])], registry: Option<(&str, &str)>) -> MaskedProject {
        MaskedProject {
            project: "odin".to_string(),
            vars: vars
                .iter()
                .map(|(k, s)| MaskedVar {
                    key: k.to_string(),
                    services: s.iter().map(|x| x.to_string()).collect(),
                })
                .collect(),
            registry: registry.map(|(r, u)| MaskedRegistry {
                registry: r.to_string(),
                username: u.to_string(),
            }),
        }
    }

    #[test]
    fn renders_deployments_env_registry_and_log_toggle() {
        let deps = [view("b1", "running")];
        let refs: Vec<&DeploymentView> = deps.iter().collect();
        let env = [masked(
            &[("SECRET", &["backend"])],
            Some(("ghcr.io", "bot")),
        )];
        let html = project_body("odin", &refs, &env);
        assert!(html.contains("b1"));
        assert!(html.contains("reg/backend:abc"));
        assert!(html.contains("SECRET"));
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("bot"));
        // masked value bullets, never a stored value
        assert!(html.contains('\u{2022}'));
        // per-service live-log stream URL
        assert!(html.contains("/p/odin/logs/b1/backend"));
        // forms redirect scope: destroy + var management under this project
        assert!(html.contains("action=\"/ui/destroy/b1\""));
        assert!(html.contains("action=\"/ui/projects/odin/vars\""));
        assert!(html.contains("action=\"/ui/projects/odin/registry\""));
    }

    #[test]
    fn escapes_failed_status_reason() {
        let deps = [view("b", "failed: <script>alert(1)</script>")];
        let refs: Vec<&DeploymentView> = deps.iter().collect();
        let html = project_body("odin", &refs, &[]);
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
