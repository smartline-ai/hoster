use std::collections::BTreeSet;
use std::fmt::Write;

use crate::engine::DeploymentView;
use crate::secrets::{MaskedAcme, MaskedProject};

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
.col.environment,.col.dns{border-left:1px solid var(--line);background:color-mix(in srgb,var(--panel-2) 55%,transparent)}
.col.registry,.col.domain,.col.certs{grid-column:1/-1;border-top:1px solid var(--line)}
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
  .col.environment,.col.dns{border-left:0;border-top:1px solid var(--line)}}
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

/// One domain's certificate status, for the TLS panel's certificate table.
/// `state` is a free-form, human-readable summary — `"valid until
/// 2026-10-01"`, `"failed: no zone found"`, `"pending"` — built by the
/// caller from [`crate::certs::CertStore`] and the renewal loop's persisted
/// state, not by this module.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CertRow {
    pub domain: String,
    pub state: String,
}

/// The dashboard: deployments and hoster-managed environment, grouped by
/// project. `env` carries only masked variables (keys + target services) —
/// values are never passed in, so they cannot be rendered. `default_template`
/// is the global hostname template, shown for any project that has not set
/// its own. `acme` and `certs` back the global TLS & DNS panel: `acme` is
/// `None` until an ACME account email has been set, and structurally cannot
/// carry the DNS token (see [`MaskedAcme`]); `certs` is the current
/// certificate table, one row per domain hoster wants a certificate for.
pub fn dashboard_page(
    deployments: &[DeploymentView],
    env: &[MaskedProject],
    default_template: &str,
    acme: Option<&MaskedAcme>,
    certs: &[CertRow],
) -> String {
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

    render_tls(&mut body, acme, certs);

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
        render_domain(&mut body, project, env, default_template);
        render_registry(&mut body, project, env);
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

/// The domain block for one project: the effective hostname template — the
/// project's own, or the global default marked as inherited — plus a form to
/// set or replace it. The template is not a secret, so unlike the environment
/// and registry rows it is rendered in full rather than masked. Like
/// `render_registry`, this reuses `render_environment`'s `env-list`/`env-row`
/// row shape for a single row instead of a multi-item list.
fn render_domain(body: &mut String, project: &str, env: &[MaskedProject], default_template: &str) {
    let own = env
        .iter()
        .find(|p| p.project == project)
        .and_then(|p| p.hostname_template.as_deref());
    let proj = html_escape(project);

    body.push_str(
        "<aside class=\"col domain\"><div class=\"col-label\">Domain</div><div class=\"env-list\">",
    );
    match own {
        None => {
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag all\">default</span></div></div>",
                html_escape(default_template),
            );
        }
        Some(t) => {
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<form method=\"post\" action=\"/ui/projects/{proj}/domain/delete\" \
onsubmit=\"return confirm('Revert this project to the default domain?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Revert to default\">\u{2715}</button></form></div>",
                html_escape(t),
            );
        }
    }
    body.push_str("</div>");

    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/projects/{proj}/domain\">\
<input name=\"hostname_template\" placeholder=\"{{branch}}.demo.example.com\" required>\
<button class=\"btn primary\" type=\"submit\">Save domain</button></form></aside>",
    );
}

/// The registry-credential block for one project: the stored host and
/// username (password masked, never rendered) plus a form to set or replace
/// it. A project has at most one credential, so this reuses `render_environment`'s
/// `env-list`/`env-row` row shape for a single row instead of a multi-item list.
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

/// The global TLS & DNS panel: the ACME account (email + optional control
/// hostname), the DNS provider credential (masked, never rendered — like
/// `render_registry`'s password row), and the per-domain certificate table.
/// Unlike the panels below it this is not per-project — one hoster instance
/// holds one ACME account — so `dashboard_page` renders it once, above the
/// project loop.
fn render_tls(body: &mut String, acme: Option<&MaskedAcme>, certs: &[CertRow]) {
    body.push_str(
        "<section class=\"panel\"><div class=\"panel-head\">\
<div class=\"panel-title\"><span class=\"proj-glyph\">\u{25c8}</span><h2>TLS &amp; DNS</h2></div>\
<div class=\"panel-meta\">Let\u{2019}s Encrypt via DNS-01</div></div><div class=\"panel-body\">",
    );

    // ACME account: email + control hostname, plus a form to set/replace them.
    body.push_str("<div class=\"col\"><div class=\"col-label\">ACME account</div>");
    match acme {
        None => body.push_str(
            "<div class=\"empty\">TLS is not configured. Set an account email to start \
issuing certificates.</div>",
        ),
        Some(a) => {
            let hostname_tag = match &a.control_hostname {
                Some(h) => format!("<span class=\"tag\">{}</span>", html_escape(h)),
                None => "<span class=\"tag\">no control hostname</span>".to_string(),
            };
            let _ = write!(
                body,
                "<div class=\"env-list\"><div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\">{hostname_tag}</div></div></div>",
                html_escape(&a.email),
            );
        }
    }
    body.push_str(
        "<form class=\"add-var\" method=\"post\" action=\"/ui/acme/config\">\
<input name=\"email\" placeholder=\"you@example.com\" required>\
<input name=\"control_hostname\" placeholder=\"hoster.example.com (optional)\">\
<button class=\"btn primary\" type=\"submit\">Save ACME account</button></form></div>",
    );

    // DNS provider: masked token (set/replace + remove), never the token
    // itself — same discipline as `render_registry`'s password row.
    body.push_str("<aside class=\"col dns\"><div class=\"col-label\">DNS provider</div>");
    match acme.and_then(|a| a.provider_kind.as_deref()) {
        None => body.push_str(
            "<div class=\"empty\">No DNS provider set. Required to issue wildcard \
certificates via DNS-01.</div>",
        ),
        Some(kind) => {
            let _ = write!(
                body,
                "<div class=\"env-list\"><div class=\"env-row\"><span class=\"k\">{}</span>\
<form method=\"post\" action=\"/ui/acme/dns/delete\" \
onsubmit=\"return confirm('Remove the DNS provider token?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Remove DNS token\">\u{2715}</button></form>\
<div class=\"env-meta\"><span class=\"val\">\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}</span>\
<span class=\"tag\">token set</span></div></div></div>",
                html_escape(kind),
            );
        }
    }
    body.push_str(
        "<form class=\"add-var\" method=\"post\" action=\"/ui/acme/dns\">\
<input name=\"kind\" placeholder=\"cloudflare\" required>\
<input name=\"token\" type=\"password\" placeholder=\"API token\" autocomplete=\"off\" required>\
<button class=\"btn primary\" type=\"submit\">Save DNS token</button></form></aside>",
    );

    // Certificates: one row per domain hoster wants a certificate for, so a
    // failure keeps serving plain HTTP visibly rather than going dark.
    // The retry affordance: without it an operator who has just entered
    // credentials watches `failed: ACME is not configured` for up to six
    // hours with no way to ask hoster to try again. Only offered once an
    // account exists, since there is nothing to retry before that.
    let retry = if acme.is_some() {
        "<form method=\"post\" action=\"/ui/acme/renew\" style=\"display:inline\">\
<button class=\"btn\" type=\"submit\">Retry now</button></form>"
    } else {
        ""
    };
    let _ = write!(
        body,
        "<div class=\"col certs\"><div class=\"col-label\">Certificates <span class=\"count\">{}</span> {retry}</div>",
        certs.len(),
    );
    if certs.is_empty() {
        body.push_str("<div class=\"empty\">No certificates yet.</div>");
    } else {
        body.push_str("<div class=\"env-list\">");
        for row in certs {
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag\">{}</span></div></div>",
                html_escape(&row.domain),
                html_escape(&row.state),
            );
        }
        body.push_str("</div>");
    }
    body.push_str("</div>");

    body.push_str("</div></section>");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::secrets::{MaskedRegistry, MaskedVar};
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
            registry: None,
            hostname_template: None,
        }
    }

    fn masked_with_registry(project: &str, registry: &str, username: &str) -> MaskedProject {
        MaskedProject {
            project: project.to_string(),
            vars: vec![],
            registry: Some(MaskedRegistry {
                registry: registry.to_string(),
                username: username.to_string(),
            }),
            hostname_template: None,
        }
    }

    fn masked_with_template(project: &str, template: &str) -> MaskedProject {
        MaskedProject {
            project: project.to_string(),
            vars: vec![],
            registry: None,
            hostname_template: Some(template.to_string()),
        }
    }

    const DEFAULT_TEMPLATE: &str = "{service}-{branch}.dev.example.com";

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
            DEFAULT_TEMPLATE,
            None,
            &[],
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
        let html = dashboard_page(
            &[view("odinvestor", "b1", "running", CFG)],
            &[],
            DEFAULT_TEMPLATE,
            None,
            &[],
        );
        assert!(html.contains("reg/backend:abc"));
        assert!(html.contains("PORT"));
        assert!(html.contains("8080"));
    }

    #[test]
    fn masked_var_shows_key_not_value_and_has_forms() {
        let html = dashboard_page(
            &[],
            &[masked("p", &[("SECRET", &[])])],
            DEFAULT_TEMPLATE,
            None,
            &[],
        );
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
        let html = dashboard_page(
            &[],
            &[masked("secretsonly", &[("K", &[])])],
            DEFAULT_TEMPLATE,
            None,
            &[],
        );
        assert!(html.contains("secretsonly"));
        assert!(html.contains("No deployments"));
    }

    #[test]
    fn dashboard_escapes_status_text() {
        let html = dashboard_page(
            &[view("p", "b", "failed: <script>alert(1)</script>", CFG)],
            &[],
            DEFAULT_TEMPLATE,
            None,
            &[],
        );
        assert!(!html.contains("<script>alert(1)"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn dashboard_empty_state() {
        let html = dashboard_page(&[], &[], DEFAULT_TEMPLATE, None, &[]);
        assert!(html.to_lowercase().contains("no projects"));
    }

    #[test]
    fn registry_row_shows_host_and_username_but_masks_the_password() {
        let env = [masked_with_registry("p", "ghcr.io", "bot")];
        let html = dashboard_page(&[], &env, DEFAULT_TEMPLATE, None, &[]);
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("bot"));
        assert!(html.contains("\u{2022}\u{2022}\u{2022}\u{2022}"));
        assert!(html.contains("action=\"/ui/projects/p/registry/delete\""));
    }

    #[test]
    fn project_without_a_registry_shows_the_empty_state_and_a_form() {
        let env = [masked("p", &[("K", &[][..])])];
        let html = dashboard_page(&[], &env, DEFAULT_TEMPLATE, None, &[]);
        assert!(html.contains("No registry credential"));
        assert!(html.contains("action=\"/ui/projects/p/registry\""));
    }

    #[test]
    fn registry_fields_are_html_escaped() {
        let env = [masked_with_registry("p", "ghcr.io", "<script>x</script>")];
        let html = dashboard_page(&[], &env, DEFAULT_TEMPLATE, None, &[]);
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn shows_a_projects_own_domain_with_a_reset_control() {
        let env = [masked_with_template("p", "{branch}.demo.example.com")];
        let html = dashboard_page(&[], &env, DEFAULT_TEMPLATE, None, &[]);
        assert!(html.contains("demo.example.com"));
        assert!(html.contains("/ui/projects/p/domain/delete"));
    }

    #[test]
    fn shows_the_global_default_as_inherited_when_unset() {
        let env = [masked("p", &[("K", &[][..])])];
        let html = dashboard_page(&[], &env, DEFAULT_TEMPLATE, None, &[]);
        assert!(html.contains("dev.example.com"));
        assert!(
            html.to_lowercase().contains("default"),
            "an inherited domain should be labelled as the default"
        );
        assert!(html.contains("action=\"/ui/projects/p/domain\""));
    }

    #[test]
    fn domain_is_html_escaped() {
        let env = [masked_with_template("p", "{branch}.<script>x</script>.com")];
        let html = dashboard_page(&[], &env, DEFAULT_TEMPLATE, None, &[]);
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn tls_section_shows_setup_prompt_when_unconfigured() {
        let html = dashboard_page(&[], &[], DEFAULT_TEMPLATE, None, &[]);
        assert!(html.to_lowercase().contains("tls"));
        assert!(html.contains("/ui/acme/config"));
    }

    #[test]
    fn tls_section_never_renders_the_token() {
        let masked = MaskedAcme {
            email: "me@example.com".into(),
            control_hostname: None,
            provider_kind: Some("cloudflare".into()),
            token_set: true,
        };
        let html = dashboard_page(&[], &[], DEFAULT_TEMPLATE, Some(&masked), &[]);
        assert!(html.contains("me@example.com"));
        assert!(html.contains("cloudflare"));
        assert!(html.contains("\u{2022}\u{2022}\u{2022}\u{2022}"));
    }

    /// A failed domain must come with a way to retry: an operator who has
    /// just entered credentials should not have to wait for the next
    /// scheduled pass to find out whether they worked.
    #[test]
    fn certificate_table_offers_a_retry_button_once_acme_is_configured() {
        let masked = MaskedAcme {
            email: "me@example.com".into(),
            control_hostname: None,
            provider_kind: Some("cloudflare".into()),
            token_set: true,
        };
        let rows = vec![CertRow {
            domain: "*.dev.example.com".into(),
            state: "failed: ACME is not configured".into(),
        }];
        let html = dashboard_page(&[], &[], DEFAULT_TEMPLATE, Some(&masked), &rows);
        assert!(html.contains("/ui/acme/renew"), "no retry affordance");

        // Nothing to retry before an account exists.
        let html = dashboard_page(&[], &[], DEFAULT_TEMPLATE, None, &rows);
        assert!(!html.contains("/ui/acme/renew"));
    }

    #[test]
    fn certificate_table_shows_state_per_domain() {
        let rows = vec![
            CertRow {
                domain: "*.dev.example.com".into(),
                state: "valid until 2026-10-01".into(),
            },
            CertRow {
                domain: "*.demo.example.com".into(),
                state: "failed: no zone found".into(),
            },
        ];
        let html = dashboard_page(&[], &[], DEFAULT_TEMPLATE, None, &rows);
        assert!(html.contains("*.dev.example.com"));
        assert!(html.contains("valid until 2026-10-01"));
        assert!(html.contains("no zone found"));
    }
}
