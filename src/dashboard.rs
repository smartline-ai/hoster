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

const STYLE: &str = r#"
:root{
  --bg:#0d1017;--panel:#141a22;--panel-2:#1a212b;--raise:#1f2732;
  --line:#242d3a;--line-2:#323d4d;--ink:#e9edf3;--muted:#8a94a6;--faint:#5b6576;
  --accent:#7b8cff;--accent-2:#a97bff;--accent-ink:#0d1017;
  --run:#3fb950;--prov:#d8a123;--fail:#f8564b;
  --run-bg:rgba(63,185,80,.13);--prov-bg:rgba(216,161,35,.14);--fail-bg:rgba(248,86,75,.13);
  --radius:14px;--radius-sm:9px;
  --mono:ui-monospace,"SF Mono","JetBrains Mono",Menlo,Consolas,monospace;
  --sans:-apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Inter,system-ui,sans-serif;
  --shadow:0 1px 2px rgba(0,0,0,.4),0 8px 24px -12px rgba(0,0,0,.55);
}
@media(prefers-color-scheme:light){:root{
  --bg:#f5f6f9;--panel:#fff;--panel-2:#f4f6f9;--raise:#fff;
  --line:#e6e9ef;--line-2:#d6dbe4;--ink:#161b24;--muted:#5b6675;--faint:#98a1b0;
  --accent:#5560e6;--accent-2:#7d4fe0;--accent-ink:#fff;
  --run:#1a7f37;--prov:#9a6700;--fail:#cf222e;
  --run-bg:rgba(26,127,55,.10);--prov-bg:rgba(154,103,0,.11);--fail-bg:rgba(207,34,46,.09);
  --shadow:0 1px 2px rgba(20,30,60,.06),0 12px 30px -18px rgba(20,30,60,.22);
}}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--ink);font-family:var(--sans);font-size:14px;line-height:1.5;
  -webkit-font-smoothing:antialiased;background-image:radial-gradient(1200px 500px at 80% -10%,rgba(123,140,255,.08),transparent 60%)}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
code{font-family:var(--mono)}h1,h2,h3{margin:0}button{font-family:inherit;cursor:pointer}
:focus-visible{outline:2px solid var(--accent);outline-offset:2px;border-radius:6px}
.topbar{position:sticky;top:0;z-index:10;display:flex;align-items:center;gap:1rem;
  padding:.85rem clamp(1rem,4vw,2.4rem);background:color-mix(in srgb,var(--bg) 82%,transparent);
  backdrop-filter:saturate(1.4) blur(10px);border-bottom:1px solid var(--line)}
.brand{display:flex;align-items:center;gap:.6rem}
.mark{width:26px;height:26px;flex:none}
.wordmark{font-weight:680;letter-spacing:-.01em;font-size:1.06rem}
.brand-sub{font-family:var(--mono);font-size:.66rem;letter-spacing:.16em;text-transform:uppercase;
  color:var(--muted);padding:.16rem .45rem;border:1px solid var(--line-2);border-radius:999px}
.top-actions{margin-left:auto;display:flex;align-items:center;gap:1rem}
.summary{color:var(--muted);font-size:.82rem}.summary b{color:var(--ink);font-weight:600}
.btn{font-size:.82rem;font-weight:560;border-radius:8px;padding:.5rem .85rem;border:1px solid var(--line-2);
  background:var(--panel-2);color:var(--ink);transition:.14s ease;line-height:1}
.btn:hover{border-color:var(--accent);transform:translateY(-1px)}
.btn.primary{border:0;color:var(--accent-ink);background:linear-gradient(135deg,var(--accent),var(--accent-2));box-shadow:0 6px 16px -8px var(--accent)}
.btn.primary:hover{filter:brightness(1.06)}.btn.ghost{background:transparent}
.btn.danger{background:transparent;border-color:transparent;color:var(--muted);padding:.4rem .55rem}
.btn.danger:hover{color:var(--fail);border-color:var(--fail);transform:none}
.icon-btn{background:transparent;border:1px solid transparent;color:var(--faint);width:26px;height:26px;
  border-radius:7px;display:grid;place-items:center;transition:.14s;font-size:.85rem}
.icon-btn:hover{color:var(--fail);border-color:var(--fail)}
main{max-width:1080px;margin:0 auto;padding:clamp(1rem,3vw,2rem) clamp(1rem,4vw,2.4rem) 4rem}
.panel{background:var(--panel);border:1px solid var(--line);border-radius:var(--radius);box-shadow:var(--shadow);margin-top:1.4rem;overflow:hidden}
.panel-head{display:flex;align-items:baseline;gap:.8rem;flex-wrap:wrap;padding:1rem 1.2rem;
  border-bottom:1px solid var(--line);background:linear-gradient(180deg,var(--panel-2),transparent)}
.panel-title{display:flex;align-items:center;gap:.55rem}.proj-glyph{color:var(--accent);font-size:.9rem}
.panel-title h2{font-size:1.02rem;font-weight:640;letter-spacing:-.01em}
.panel-meta{color:var(--muted);font-size:.78rem;font-family:var(--mono)}
.panel-body{display:grid;grid-template-columns:1.55fr 1fr;gap:0}
.col{padding:1rem 1.2rem}
.col.environment{border-left:1px solid var(--line);background:color-mix(in srgb,var(--panel-2) 55%,transparent)}
.col-label{font-size:.68rem;letter-spacing:.15em;text-transform:uppercase;color:var(--muted);font-weight:600;
  margin-bottom:.7rem;display:flex;align-items:center;gap:.5rem}
.col-label .count{color:var(--faint);font-family:var(--mono);letter-spacing:0}
.deploy{position:relative;display:flex;gap:.7rem;align-items:flex-start;padding:.8rem .8rem .8rem .95rem;
  border:1px solid var(--line);border-left-width:3px;border-radius:var(--radius-sm);background:var(--raise);margin-bottom:.6rem;transition:.14s}
.deploy:hover{border-color:var(--line-2)}
.deploy.is-running{border-left-color:var(--run)}.deploy.is-provisioning{border-left-color:var(--prov)}.deploy.is-failed{border-left-color:var(--fail)}
.led{width:8px;height:8px;border-radius:50%;margin-top:.42rem;flex:none;background:var(--faint)}
.is-running .led{background:var(--run);animation:pulse 2.4s infinite}
.is-provisioning .led{background:var(--prov);animation:pulse 1.3s infinite}
.is-failed .led{background:var(--fail)}
@keyframes pulse{0%{box-shadow:0 0 0 0 color-mix(in srgb,var(--run) 60%,transparent)}70%{box-shadow:0 0 0 6px transparent}100%{box-shadow:0 0 0 0 transparent}}
.deploy-main{flex:1;min-width:0}
.deploy-row1{display:flex;align-items:center;gap:.6rem;flex-wrap:wrap}
.branch{font-family:var(--mono);font-weight:600;font-size:.9rem;letter-spacing:-.01em}
.pill{display:inline-flex;align-items:center;gap:.35rem;font-size:.7rem;font-weight:600;padding:.16rem .5rem;border-radius:999px}
.pill .dot{width:5px;height:5px;border-radius:50%;background:currentColor}
.pill.running{color:var(--run);background:var(--run-bg)}
.pill.provisioning{color:var(--prov);background:var(--prov-bg)}
.pill.failed{color:var(--fail);background:var(--fail-bg)}
.urls{display:flex;flex-wrap:wrap;gap:.35rem;margin-top:.5rem}
.chip{display:inline-flex;align-items:center;gap:.35rem;min-width:0;max-width:100%;font-family:var(--mono);
  font-size:.76rem;color:var(--ink);padding:.24rem .55rem;border:1px solid var(--line-2);border-radius:7px;background:var(--panel)}
.chip:hover{border-color:var(--accent);text-decoration:none;color:var(--accent)}
.chip svg{width:11px;height:11px;opacity:.6;flex:none}
.chip .host{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.reason{margin-top:.5rem;font-size:.78rem;color:var(--fail);font-family:var(--mono)}
details.config{margin-top:.6rem}
details.config>summary{list-style:none;cursor:pointer;display:inline-flex;align-items:center;gap:.35rem;
  font-size:.75rem;color:var(--muted);font-weight:560;user-select:none}
details.config>summary::-webkit-details-marker{display:none}
.chev{transition:.15s;color:var(--faint)}details[open] .chev{transform:rotate(90deg)}
.svc-grid{display:grid;gap:.5rem;margin-top:.55rem}
.svc{border:1px solid var(--line);border-radius:8px;padding:.6rem .7rem;background:var(--panel-2)}
.svc-head{display:flex;align-items:center;gap:.5rem;margin-bottom:.35rem}
.svc-name{font-weight:620;font-size:.82rem}
.port{font-family:var(--mono);font-size:.68rem;color:var(--accent);background:color-mix(in srgb,var(--accent) 14%,transparent);padding:.06rem .38rem;border-radius:5px}
.svc .img{display:block;font-size:.75rem;color:var(--muted);word-break:break-all}
.env-inline{margin:.45rem 0 0;padding:0;list-style:none;display:grid;gap:.15rem}
.env-inline li{font-family:var(--mono);font-size:.73rem;color:var(--muted);word-break:break-all}
.env-inline .k{color:var(--ink)}.env-inline .eq{color:var(--faint)}
.env-list{display:grid;gap:.3rem;margin-bottom:.9rem}
.env-row{display:grid;grid-template-columns:1fr auto;align-items:center;gap:.3rem .6rem;
  padding:.5rem .6rem;border:1px solid var(--line);border-radius:8px;background:var(--raise)}
.env-row .k{font-family:var(--mono);font-size:.79rem;font-weight:600;color:var(--ink);word-break:break-all}
.env-row .val{font-family:var(--mono);color:var(--faint);letter-spacing:.05em;font-size:.8rem}
.env-meta{grid-column:1/2;display:flex;align-items:center;gap:.35rem;flex-wrap:wrap;margin-top:.15rem}
.env-row form{grid-row:1/3;grid-column:2;align-self:center}
.tag{font-family:var(--mono);font-size:.68rem;color:var(--muted);background:var(--panel-2);border:1px solid var(--line);padding:.05rem .4rem;border-radius:5px}
.tag.all{color:var(--accent);border-color:color-mix(in srgb,var(--accent) 35%,var(--line))}
.add-var{display:grid;grid-template-columns:1fr;gap:.45rem;padding:.8rem;border:1px dashed var(--line-2);border-radius:9px}
.add-var input{width:100%;font-family:var(--mono);font-size:.8rem;color:var(--ink);background:var(--panel);
  border:1px solid var(--line-2);border-radius:7px;padding:.5rem .6rem}
.add-var input::placeholder{color:var(--faint)}
.add-var input:focus{outline:none;border-color:var(--accent)}
.empty{color:var(--muted);font-size:.82rem;padding:.9rem;border:1px dashed var(--line-2);border-radius:9px;text-align:center}
.login-wrap{min-height:100dvh;display:grid;place-items:center;padding:1.5rem}
.login-card{width:100%;max-width:360px;background:var(--panel);border:1px solid var(--line);border-radius:var(--radius);
  box-shadow:var(--shadow);padding:2rem 1.8rem;text-align:center}
.login-card .mark{width:40px;height:40px;margin:0 auto .8rem}
.login-card h1{font-size:1.25rem;letter-spacing:-.02em}
.login-card p{color:var(--muted);font-size:.85rem;margin:.35rem 0 1.4rem}
.login-form{display:grid;gap:.6rem}
.login-form input{font-size:.9rem;padding:.7rem .8rem;border-radius:9px;border:1px solid var(--line-2);
  background:var(--panel-2);color:var(--ink);text-align:center}
.login-form input:focus{outline:none;border-color:var(--accent)}
.login-form .btn.primary{padding:.7rem}
.err{color:var(--fail);font-size:.82rem;margin:0 0 .4rem}
@media(max-width:760px){.panel-body{grid-template-columns:1fr}
  .col.environment{border-left:0;border-top:1px solid var(--line)}}
@media(prefers-reduced-motion:reduce){*{animation:none!important;transition:none!important}}
"#;

/// The brand mark: a single host (gradient node) fanning out to three branch
/// endpoints — hoster's whole job in one glyph.
const MARK: &str = r##"<svg class="mark" viewBox="0 0 32 32" fill="none" aria-hidden="true"><defs><linearGradient id="hg" x1="0" y1="0" x2="32" y2="32"><stop stop-color="#7b8cff"/><stop offset="1" stop-color="#a97bff"/></linearGradient></defs><circle cx="6" cy="16" r="3.2" fill="url(#hg)"/><circle cx="26" cy="7" r="2.6" fill="currentColor" opacity=".85"/><circle cx="26" cy="16" r="2.6" fill="currentColor" opacity=".85"/><circle cx="26" cy="25" r="2.6" fill="currentColor" opacity=".85"/><path d="M9 16H16M16 16V7H23M16 16H23M16 16V25H23" stroke="url(#hg)" stroke-width="1.6" stroke-linecap="round"/></svg>"##;

const EXT_ICON: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true"><path d="M7 17 17 7M9 7h8v8"/></svg>"#;

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{STYLE}</style></head><body>{body}</body></html>",
        html_escape(title)
    )
}

/// `1 branch` / `2 branches` — plain-English counts for the panel meta line.
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The login form. `error` renders a message above the form when a prior
/// attempt failed.
pub fn login_page(error: Option<&str>) -> String {
    let err = error
        .map(|e| format!("<p class=\"err\">{}</p>", html_escape(e)))
        .unwrap_or_default();
    let body = format!(
        "<div class=\"login-wrap\"><div class=\"login-card\">{MARK}\
<h1>hoster</h1><p>Sign in to the deploy console.</p>{err}\
<form class=\"login-form\" method=\"post\" action=\"/login\">\
<input type=\"password\" name=\"password\" placeholder=\"Password\" autocomplete=\"current-password\" autofocus>\
<button class=\"btn primary\" type=\"submit\">Sign in</button></form></div></div>"
    );
    page("hoster — sign in", &body)
}

fn is_running(status: &str) -> bool {
    status == "running"
}

/// The dashboard: deployments and hoster-managed environment, grouped by
/// project. `env` carries only masked variables (keys + target services) —
/// values are never passed in, so they cannot be rendered.
pub fn dashboard_page(deployments: &[DeploymentView], env: &[MaskedProject]) -> String {
    let mut projects: BTreeSet<&str> = BTreeSet::new();
    for d in deployments {
        projects.insert(d.project.as_str());
    }
    for p in env {
        projects.insert(p.project.as_str());
    }

    let running = deployments.iter().filter(|d| is_running(&d.status)).count();
    let mut body = format!(
        "<header class=\"topbar\"><div class=\"brand\">{MARK}\
<span class=\"wordmark\">hoster</span><span class=\"brand-sub\">deploy console</span></div>\
<div class=\"top-actions\"><span class=\"summary\"><b>{}</b> {} · <b>{running}</b> running</span>\
<form method=\"post\" action=\"/logout\"><button class=\"btn ghost\" type=\"submit\">Sign out</button></form>\
</div></header><main>",
        projects.len(),
        if projects.len() == 1 {
            "project"
        } else {
            "projects"
        },
    );

    if projects.is_empty() {
        body.push_str(
            "<div class=\"empty\" style=\"margin-top:2rem\">No projects yet. \
Deploy a branch or add environment variables to get started.</div>",
        );
    }

    for project in projects {
        let deps: Vec<&DeploymentView> = deployments
            .iter()
            .filter(|d| d.project == project)
            .collect();
        let vars = env
            .iter()
            .find(|p| p.project == project)
            .map(|p| p.vars.len())
            .unwrap_or(0);
        let run = deps.iter().filter(|d| is_running(&d.status)).count();
        let esc = html_escape(project);

        let _ = write!(
            body,
            "<section class=\"panel\"><div class=\"panel-head\">\
<div class=\"panel-title\"><span class=\"proj-glyph\">\u{25c8}</span><h2>{esc}</h2></div>\
<div class=\"panel-meta\">{} · {} · {}</div></div><div class=\"panel-body\">",
            plural(deps.len(), "branch", "branches"),
            plural(run, "running", "running"),
            plural(vars, "variable", "variables"),
        );

        render_deployments(&mut body, &deps);
        render_environment(&mut body, project, env);
        body.push_str("</div></section>");
    }

    body.push_str("</main>");
    page("hoster — dashboard", &body)
}

/// The deployments column for one project: each branch's status, URLs, and an
/// expandable view of the config it was deployed from.
fn render_deployments(body: &mut String, deps: &[&DeploymentView]) {
    let _ = write!(
        body,
        "<div class=\"col\"><div class=\"col-label\">Deployments <span class=\"count\">{}</span></div>",
        deps.len()
    );
    if deps.is_empty() {
        body.push_str("<div class=\"empty\">No deployments yet.</div></div>");
        return;
    }
    for d in deps {
        let branch = html_escape(&d.branch);
        // Status is either a word ("running") or "word: reason" (failed).
        let (word, reason) = match d.status.split_once(':') {
            Some((w, r)) => (w.trim(), Some(r.trim()).filter(|r| !r.is_empty())),
            None => (d.status.trim(), None),
        };

        let _ = write!(
            body,
            "<article class=\"deploy is-{word}\"><span class=\"led\"></span><div class=\"deploy-main\">\
<div class=\"deploy-row1\"><span class=\"branch\">{branch}</span>\
<span class=\"pill {word}\"><span class=\"dot\"></span>{word}</span></div>",
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
                    "<a class=\"chip\" href=\"{e}\"><span class=\"host\">{e}</span>{EXT_ICON}</a>",
                );
            }
            body.push_str("</div>");
        }

        render_config(body, d);
        let _ = write!(
            body,
            "</div><form class=\"deploy-actions\" method=\"post\" action=\"/ui/destroy/{branch}\" \
onsubmit=\"return confirm('Destroy this branch?')\">\
<button class=\"btn danger\" type=\"submit\" title=\"Destroy branch\">Destroy</button></form></article>",
        );
    }
    body.push_str("</div>");
}

/// The `<details>` config view for one deployment: per service the image,
/// exposed port, and its `hoster.json` env (shown in plaintext).
fn render_config(body: &mut String, d: &DeploymentView) {
    let Some(cfg) = &d.config else {
        body.push_str(
            "<p class=\"reason\" style=\"color:var(--faint)\">configuration unavailable</p>",
        );
        return;
    };
    body.push_str(
        "<details class=\"config\"><summary><span class=\"chev\">\u{203a}</span> configuration</summary>\
<div class=\"svc-grid\">",
    );
    for (name, svc) in &cfg.services {
        let _ = write!(
            body,
            "<div class=\"svc\"><div class=\"svc-head\"><span class=\"svc-name\">{}</span>",
            html_escape(name),
        );
        if let Some(exp) = &svc.expose {
            let _ = write!(body, "<span class=\"port\">:{}</span>", exp.port);
        }
        let _ = write!(
            body,
            "</div><code class=\"img\">{}</code>",
            html_escape(&svc.image),
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
        body.push_str("</div>");
    }
    body.push_str("</div></details>");
}

/// The environment column for one project: its masked vars (with delete forms)
/// and a form to add another.
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
            "<div class=\"empty\">No variables yet. Add one below and it's injected \
into every deploy of this project.</div>",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::secrets::MaskedVar;
    use std::collections::BTreeMap;

    fn view(project: &str, branch: &str, status: &str, config_json: &str) -> DeploymentView {
        DeploymentView {
            project: project.to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            urls: BTreeMap::new(),
            config: config::parse(config_json).ok(),
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
        assert!(html.contains("GOOGLE_API_KEY"));
        assert!(html.contains("backend"));
        assert!(html.contains("/ui/destroy/b1"));
    }

    #[test]
    fn shows_service_image_and_env_from_config() {
        let html = dashboard_page(&[view("odinvestor", "b1", "running", CFG)], &[]);
        assert!(html.contains("reg/backend:abc"));
        assert!(html.contains("PORT"));
        assert!(html.contains("8080"));
    }

    #[test]
    fn masked_var_shows_key_not_value_and_has_forms() {
        let html = dashboard_page(&[], &[masked("p", &[("SECRET", &[])])]);
        assert!(html.contains("SECRET"));
        assert!(html.contains("action=\"/ui/projects/p/vars\""));
        assert!(html.contains("/ui/projects/p/vars/SECRET/delete"));
        // masked bullets, never a value
        assert!(html.contains('\u{2022}'));
        // an untargeted var reads as "all services"
        assert!(html.contains("all services"));
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
