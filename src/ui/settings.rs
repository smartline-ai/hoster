//! The Settings page body.
use std::fmt::Write;

use crate::settings::Settings;
use crate::ui::components::html_escape;

pub fn settings_body(settings: &Settings) -> String {
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
    body
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
        }
    }

    #[test]
    fn shows_system_info_but_never_secrets() {
        let html = settings_body(&settings());
        assert!(html.contains("{service}-{branch}.dev.example.com"));
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("0.0.0.0:8081"));
        // secrets must never render
        assert!(!html.contains("super-secret-token"));
        assert!(!html.contains("hunter2"));
    }
}
