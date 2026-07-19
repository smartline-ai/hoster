//! The persistent app frame: a left navigation rail + right content pane.

use std::fmt::Write;

use crate::ui::components::{MARK, html_escape};
use crate::ui::page;

/// Which nav item is active on the current page.
pub enum Nav<'a> {
    Overview,
    Project(&'a str),
    Settings,
}

/// Render the full page: sidebar rail (Overview, projects, Settings, Sign out)
/// wrapping the given content on the right. The active item is highlighted.
pub fn app_shell(active: Nav, projects: &[&str], content: &str) -> String {
    let mut rail = format!(
        "<nav class=\"rail\"><a class=\"brand\" href=\"/\">{MARK}<span class=\"wordmark\">hoster</span></a>",
    );

    let overview_cls = if matches!(active, Nav::Overview) {
        "nav-item active"
    } else {
        "nav-item"
    };
    let _ = write!(
        rail,
        "<a class=\"{overview_cls}\" href=\"/\"><span class=\"glyph\">\u{25d0}</span>Overview</a>",
    );

    rail.push_str("<div class=\"nav-label\">Projects</div>");
    if projects.is_empty() {
        rail.push_str("<span class=\"nav-item\" style=\"color:var(--faint)\">None yet</span>");
    }
    for p in projects {
        let active_here = matches!(active, Nav::Project(cur) if cur == *p);
        let cls = if active_here {
            "nav-item active"
        } else {
            "nav-item"
        };
        let esc = html_escape(p);
        let _ = write!(
            rail,
            "<a class=\"{cls}\" href=\"/p/{esc}\"><span class=\"glyph\">\u{25c8}</span>{esc}</a>",
        );
    }

    let settings_cls = if matches!(active, Nav::Settings) {
        "nav-item active"
    } else {
        "nav-item"
    };
    let _ = write!(
        rail,
        "<div class=\"nav-spacer\"></div>\
<a class=\"{settings_cls}\" href=\"/settings\"><span class=\"glyph\">\u{2699}</span>Settings</a>\
<form method=\"post\" action=\"/logout\"><button class=\"nav-item\" type=\"submit\" \
style=\"width:100%;background:none;border:0;text-align:left\">\
<span class=\"glyph\">\u{21aa}</span>Sign out</button></form></nav>",
    );

    let body = format!("<div class=\"app\">{rail}<main class=\"content\">{content}</main></div>");
    page("hoster", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_lists_projects_and_marks_the_active_one() {
        let html = app_shell(Nav::Project("blog"), &["blog", "api"], "BODY");
        assert!(html.contains("href=\"/p/blog\""));
        assert!(html.contains("href=\"/p/api\""));
        // the active project's link carries the active class
        assert!(html.contains("nav-item active\" href=\"/p/blog\""));
        assert!(html.contains("BODY"));
        assert!(html.contains("action=\"/logout\""));
    }

    #[test]
    fn overview_is_active_on_the_overview_shell() {
        let html = app_shell(Nav::Overview, &["blog"], "X");
        assert!(html.contains("nav-item active\" href=\"/\""));
    }
}
