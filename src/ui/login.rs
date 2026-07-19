//! The sign-in page — the one view rendered outside the app shell.

use crate::ui::components::MARK;
use crate::ui::{html_escape, page};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_page_has_password_form() {
        let html = login_page(None);
        assert!(html.contains("action=\"/login\""));
        assert!(html.contains("type=\"password\""));
    }

    #[test]
    fn login_page_shows_error() {
        assert!(login_page(Some("Invalid password")).contains("Invalid password"));
    }
}
