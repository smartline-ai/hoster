//! The Overview page body — filled in Task 5.
use std::fmt::Write;

use crate::engine::DeploymentView;
use crate::ui::components::{EXT_ICON, html_escape};

/// Reduce a status string to its leading word (`"failed: boom"` -> `"failed"`).
pub(crate) fn status_word(status: &str) -> &str {
    match status.split_once(':') {
        Some((w, _)) => w.trim(),
        None => status.trim(),
    }
}

pub fn overview_body(deployments: &[DeploymentView]) -> String {
    let projects: std::collections::BTreeSet<&str> =
        deployments.iter().map(|d| d.project.as_str()).collect();
    let running = deployments
        .iter()
        .filter(|d| status_word(&d.status) == "running")
        .count();

    let mut body = format!(
        "<div class=\"page-head\"><h1>Overview</h1></div>\
<div class=\"stat-row\">\
<div class=\"stat\"><span class=\"n\">{}</span><span class=\"l\">Projects</span></div>\
<div class=\"stat\"><span class=\"n\">{}</span><span class=\"l\">Deployments</span></div>\
<div class=\"stat\"><span class=\"n\">{running}</span><span class=\"l\">Running</span></div>\
</div>",
        projects.len(),
        deployments.len(),
    );

    if deployments.is_empty() {
        body.push_str(
            "<div class=\"empty\" style=\"margin-top:1.4rem\">No deployments yet. \
Deploy a branch or add environment variables to a project to get started.</div>",
        );
        return body;
    }

    body.push_str("<div class=\"col-label\" style=\"margin-top:1.6rem\">All deployments</div>");
    for d in deployments {
        let word = html_escape(status_word(&d.status));
        let project = html_escape(&d.project);
        let branch = html_escape(&d.branch);
        let _ = write!(
            body,
            "<a class=\"deploy is-{word}\" href=\"/p/{project}\" style=\"text-decoration:none\">\
<span class=\"led\"></span><div class=\"deploy-main\"><div class=\"deploy-row1\">\
<span class=\"branch\">{branch}</span><span class=\"pill {word}\"><span class=\"dot\"></span>{word}</span>\
<span class=\"panel-meta\">{project}</span></div>",
        );
        if word != "failed" && !d.urls.is_empty() {
            body.push_str("<div class=\"urls\">");
            for u in d.urls.values() {
                let e = html_escape(u);
                let _ = write!(
                    body,
                    "<span class=\"chip\"><span class=\"host\">{e}</span>{EXT_ICON}</span>",
                );
            }
            body.push_str("</div>");
        }
        body.push_str("</div></a>");
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn view(project: &str, branch: &str, status: &str) -> DeploymentView {
        DeploymentView {
            project: project.to_string(),
            branch: branch.to_string(),
            status: status.to_string(),
            urls: BTreeMap::new(),
            config: None,
        }
    }

    #[test]
    fn overview_counts_and_lists_across_projects() {
        let body = overview_body(&[
            view("blog", "main", "running"),
            view("api", "feat-x", "failed: boom"),
        ]);
        assert!(body.contains("blog"));
        assert!(body.contains("api"));
        assert!(body.contains("href=\"/p/blog\""));
        assert!(body.contains("href=\"/p/api\""));
        // aggregate running count is present
        assert!(body.contains("Running"));
        // the computed running count (1) is rendered in the stat tile, not just the label
        assert!(body.contains("<span class=\"n\">1</span>"));
        // a failed deploy still lists, its reason escaped/omitted from status word
        assert!(body.contains("failed"));
        assert!(!body.contains("boom"));
    }
}
