//! nginx-mode config generation. See docs/superpowers/specs/2026-07-19-reverse-proxy-backend-design.md.

use std::path::PathBuf;

/// One wildcard base (or plain control hostname) served by nginx, with the
/// on-disk cert it presents. `cert_path` and `key_path` may be the same
/// combined PEM (hoster stores chain+key together in one `cert.pem`).
pub struct NginxBase {
    pub server_name: String,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// nginx `server_name` for a wanted domain. A wildcard `*.dev.example.com`
/// becomes `.dev.example.com`, which nginx matches for the parent and every
/// subdomain — exactly the set the wildcard cert covers. A plain name is used
/// verbatim.
pub fn server_name_for(domain: &str) -> String {
    match domain.strip_prefix("*.") {
        Some(parent) => format!(".{parent}"),
        None => domain.to_string(),
    }
}

/// Whether a rendered `server_name` is safe to write into the config file.
/// Operator-controlled bases are the only source, but this blocks any value
/// that could break out of the directive (whitespace, `;`, `{`, newlines).
pub fn is_safe_server_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

/// Render the full contents of hoster's nginx conf file: one shared `:80`
/// block, then one `:443` block per base. A base whose `server_name` fails
/// [`is_safe_server_name`] is skipped and logged, so nothing unexpected is
/// ever written.
pub fn render(bases: &[NginxBase], upstream: &str) -> String {
    let mut out = String::new();
    out.push_str("# Managed by hoster. Do not edit — regenerated on startup and cert renewal.\n\n");
    out.push_str(&http_block(upstream));
    for b in bases {
        if !is_safe_server_name(&b.server_name) {
            tracing::warn!(server_name = %b.server_name, "skipping unsafe nginx server_name");
            continue;
        }
        out.push('\n');
        out.push_str(&https_block(b, upstream));
    }
    out
}

fn proxy_body(upstream: &str) -> String {
    format!(
        "    location / {{\n\
         \x20       proxy_pass http://{upstream};\n\
         \x20       proxy_set_header Host $host;\n\
         \x20       proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;\n\
         \x20       proxy_set_header X-Forwarded-Proto $scheme;\n\
         \x20   }}\n"
    )
}

fn http_block(upstream: &str) -> String {
    format!(
        "server {{\n    listen 80;\n    listen [::]:80;\n    server_name _;\n{}}}\n",
        proxy_body(upstream)
    )
}

fn https_block(b: &NginxBase, upstream: &str) -> String {
    format!(
        "server {{\n    listen 443 ssl;\n    listen [::]:443 ssl;\n    http2 on;\n    \
         server_name {};\n    ssl_certificate {};\n    ssl_certificate_key {};\n{}}}\n",
        b.server_name,
        b.cert_path.display(),
        b.key_path.display(),
        proxy_body(upstream)
    )
}

#[cfg(test)]
mod render_tests {
    use super::*;

    fn base(name: &str) -> NginxBase {
        NginxBase {
            server_name: server_name_for(name),
            cert_path: PathBuf::from(format!("/certs/{name}/cert.pem")),
            key_path: PathBuf::from(format!("/certs/{name}/cert.pem")),
        }
    }

    #[test]
    fn server_name_for_wildcard_becomes_leading_dot() {
        assert_eq!(server_name_for("*.dev.example.com"), ".dev.example.com");
        assert_eq!(server_name_for("ctl.example.com"), "ctl.example.com");
    }

    #[test]
    fn render_emits_shared_port_80_block_proxying_to_upstream() {
        let out = render(&[], "127.0.0.1:8080");
        assert!(out.contains("listen 80;"), "{out}");
        assert!(out.contains("proxy_pass http://127.0.0.1:8080;"), "{out}");
        assert!(out.contains("proxy_set_header Host $host;"), "{out}");
    }

    #[test]
    fn render_emits_one_443_block_per_base_with_cert_paths() {
        let out = render(&[base("*.dev.example.com")], "127.0.0.1:8080");
        assert!(out.contains("listen 443 ssl;"), "{out}");
        assert!(out.contains("http2 on;"), "{out}");
        assert!(out.contains("server_name .dev.example.com;"), "{out}");
        assert!(out.contains("ssl_certificate /certs/*.dev.example.com/cert.pem;"), "{out}");
        assert!(out.contains("ssl_certificate_key /certs/*.dev.example.com/cert.pem;"), "{out}");
    }

    #[test]
    fn is_safe_server_name_rejects_injection() {
        assert!(is_safe_server_name(".dev.example.com"));
        assert!(!is_safe_server_name("evil.com;\n}"));
        assert!(!is_safe_server_name("has space"));
        assert!(!is_safe_server_name(""));
    }
}
