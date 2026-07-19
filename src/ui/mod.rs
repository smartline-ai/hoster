//! The operator dashboard UI: a server-rendered, sidebar-navigation console.
//! Each view lives in its own submodule; this module owns the HTML shell and
//! the public entry points the API layer calls.

mod components;
mod login;
mod overview;
mod project;
mod settings;
mod shell;
mod style;

pub use components::html_escape;
pub use login::login_page;

use crate::engine::DeploymentView;
use crate::secrets::MaskedProject;
use crate::settings::Settings;
use shell::{Nav, app_shell};

/// Wrap a rendered body in the full HTML document with inlined styles.
pub fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>{}</title><style>{}</style></head><body>{body}</body></html>",
        html_escape(title),
        style::STYLE,
    )
}

/// Sorted, de-duplicated project names across deployments and env — the
/// sidebar's project list. Shared by every page so the rail is identical.
fn project_names<'a>(deployments: &'a [DeploymentView], env: &'a [MaskedProject]) -> Vec<&'a str> {
    let mut set = std::collections::BTreeSet::new();
    for d in deployments {
        set.insert(d.project.as_str());
    }
    for p in env {
        set.insert(p.project.as_str());
    }
    set.into_iter().collect()
}

/// `GET /` — the Overview page.
pub fn overview_page(deployments: &[DeploymentView], env: &[MaskedProject]) -> String {
    let projects = project_names(deployments, env);
    let body = overview::overview_body(deployments);
    app_shell(Nav::Overview, &projects, &body)
}

/// `GET /p/{project}` — one project's deployments, env, and registry.
pub fn project_page(
    project: &str,
    deployments: &[DeploymentView],
    env: &[MaskedProject],
) -> String {
    let projects = project_names(deployments, env);
    let deps: Vec<&DeploymentView> = deployments
        .iter()
        .filter(|d| d.project == project)
        .collect();
    let body = project::project_body(project, &deps, env);
    app_shell(Nav::Project(project), &projects, &body)
}

/// `GET /settings` — read-only system information.
pub fn settings_page(
    settings: &Settings,
    deployments: &[DeploymentView],
    env: &[MaskedProject],
) -> String {
    let projects = project_names(deployments, env);
    let body = settings::settings_body(settings);
    app_shell(Nav::Settings, &projects, &body)
}
