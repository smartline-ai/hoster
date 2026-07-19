//! nginx-mode config generation. See docs/superpowers/specs/2026-07-19-reverse-proxy-backend-design.md.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Context;

use crate::certs::{write_atomic, CertStore};

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

pub struct CmdOutput {
    pub success: bool,
    pub stderr: String,
}

/// Runs one external command (argv slice) and reports success + captured
/// stderr. The seam that lets tests drive `apply` without a real nginx —
/// mirrors `Engine::with_dns_provider_builder`.
pub type CommandRunner = Box<dyn Fn(&[&str]) -> anyhow::Result<CmdOutput> + Send + Sync>;

pub struct ApplyOutcome {
    pub validated: bool,
    pub reloaded: bool,
    /// Captured stderr from `nginx -t` or the reload command, when either failed.
    pub message: Option<String>,
}

pub struct NginxBackend {
    conf_path: PathBuf,
    reload_cmd: Vec<String>,
    runner: CommandRunner,
}

fn real_runner(args: &[&str]) -> anyhow::Result<CmdOutput> {
    let (cmd, rest) = args.split_first().context("empty command")?;
    let out = std::process::Command::new(cmd)
        .args(rest)
        .output()
        .with_context(|| format!("spawn {cmd}"))?;
    Ok(CmdOutput {
        success: out.status.success(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

impl NginxBackend {
    pub fn new(conf_path: PathBuf, reload_cmd: Vec<String>) -> NginxBackend {
        NginxBackend {
            conf_path,
            reload_cmd,
            runner: Box::new(real_runner),
        }
    }

    #[cfg(test)]
    pub fn with_runner(
        conf_path: PathBuf,
        reload_cmd: Vec<String>,
        runner: CommandRunner,
    ) -> NginxBackend {
        NginxBackend {
            conf_path,
            reload_cmd,
            runner,
        }
    }

    /// Write `config`, validate with `nginx -t`, and reload on success.
    /// A failed validate — or a failure to even run `nginx -t` — restores the
    /// previous file and never reloads.
    pub fn apply(&self, config: &str) -> anyhow::Result<ApplyOutcome> {
        let backup = std::fs::read(&self.conf_path).ok();
        write_atomic(&self.conf_path, config.as_bytes(), 0o644)
            .with_context(|| format!("write {}", self.conf_path.display()))?;

        let validate = match (self.runner)(&["nginx", "-t"]) {
            Ok(v) => v,
            Err(e) => {
                self.restore_or_clear(&backup);
                return Err(e).context("run nginx -t");
            }
        };
        if !validate.success {
            self.restore_or_clear(&backup);
            return Ok(ApplyOutcome {
                validated: false,
                reloaded: false,
                message: Some(validate.stderr),
            });
        }

        let reload_refs: Vec<&str> = self.reload_cmd.iter().map(String::as_str).collect();
        let reload = (self.runner)(&reload_refs)?;
        Ok(ApplyOutcome {
            validated: true,
            reloaded: reload.success,
            message: if reload.success {
                None
            } else {
                Some(reload.stderr)
            },
        })
    }

    /// Put the config file back the way it was before `apply` wrote it:
    /// restore the previous contents, or remove the file if there was none.
    fn restore_or_clear(&self, backup: &Option<Vec<u8>>) {
        match backup {
            Some(bytes) => {
                let _ = write_atomic(&self.conf_path, bytes, 0o644);
            }
            None => {
                let _ = std::fs::remove_file(&self.conf_path);
            }
        }
    }
}

/// Snapshot of the outcome of the most recent [`NginxManager::apply_now`]
/// call, kept for the dashboard/status endpoint.
#[derive(Clone)]
pub struct ApplyRecord {
    pub validated: bool,
    pub reloaded: bool,
    pub message: Option<String>,
    pub at: i64,
}

impl ApplyRecord {
    fn from_outcome(o: &ApplyOutcome, at: i64) -> ApplyRecord {
        ApplyRecord {
            validated: o.validated,
            reloaded: o.reloaded,
            message: o.message.clone(),
            at,
        }
    }

    fn error(msg: String, at: i64) -> ApplyRecord {
        ApplyRecord {
            validated: false,
            reloaded: false,
            message: Some(msg),
            at,
        }
    }
}

/// Shared, mutable snapshot of the last `apply` result, read by the dashboard.
pub type NginxStatusHandle = Arc<Mutex<Option<ApplyRecord>>>;

/// One [`NginxBase`] per wanted domain that already has a cert on disk. A
/// domain without a cert is omitted, so `nginx -t` still passes mid-issuance.
pub fn bases_for(wanted: &[String], store: &CertStore) -> Vec<NginxBase> {
    wanted
        .iter()
        .filter_map(|domain| {
            let cert = store.dir_for(domain).join("cert.pem");
            if cert.exists() {
                Some(NginxBase {
                    server_name: server_name_for(domain),
                    cert_path: cert.clone(),
                    key_path: cert,
                })
            } else {
                None
            }
        })
        .collect()
}

type WantedFn = Box<dyn Fn() -> Vec<String> + Send + Sync>;

/// Ties the pieces together: recompute bases from the current wanted-domain set
/// and on-disk certs, render, apply, and record the outcome. Called at startup
/// and from the renewal loop's change hook — never per deploy.
pub struct NginxManager {
    backend: NginxBackend,
    cert_store: Arc<CertStore>,
    wanted: WantedFn,
    upstream: String,
    status: NginxStatusHandle,
}

impl NginxManager {
    pub fn new(
        backend: NginxBackend,
        cert_store: Arc<CertStore>,
        wanted: WantedFn,
        upstream: String,
        status: NginxStatusHandle,
    ) -> NginxManager {
        NginxManager {
            backend,
            cert_store,
            wanted,
            upstream,
            status,
        }
    }

    pub fn apply_now(&self) {
        let bases = bases_for(&(self.wanted)(), &self.cert_store);
        let config = render(&bases, &self.upstream);
        let at = crate::renewal::now_secs();
        let record = match self.backend.apply(&config) {
            Ok(o) => {
                if o.validated && o.reloaded {
                    tracing::info!(bases = bases.len(), "nginx config applied");
                } else {
                    tracing::error!(message = ?o.message, "nginx apply did not fully succeed");
                }
                ApplyRecord::from_outcome(&o, at)
            }
            Err(e) => {
                tracing::error!(error = %e, "nginx apply errored");
                ApplyRecord::error(e.to_string(), at)
            }
        };
        *self.status.lock().unwrap() = Some(record);
    }
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
        assert!(
            out.contains("ssl_certificate /certs/*.dev.example.com/cert.pem;"),
            "{out}"
        );
        assert!(
            out.contains("ssl_certificate_key /certs/*.dev.example.com/cert.pem;"),
            "{out}"
        );
    }

    #[test]
    fn is_safe_server_name_rejects_injection() {
        assert!(is_safe_server_name(".dev.example.com"));
        assert!(!is_safe_server_name("evil.com;\n}"));
        assert!(!is_safe_server_name("has space"));
        assert!(!is_safe_server_name(""));
    }

    #[test]
    fn render_skips_base_with_unsafe_server_name_but_keeps_others() {
        let unsafe_base = NginxBase {
            server_name: "evil.com;\n}".to_string(),
            cert_path: PathBuf::from("/certs/evil.com/cert.pem"),
            key_path: PathBuf::from("/certs/evil.com/cert.pem"),
        };
        let out = render(&[unsafe_base, base("*.dev.example.com")], "127.0.0.1:8080");
        assert!(!out.contains("evil.com"), "{out}");
        assert!(out.contains("server_name .dev.example.com;"), "{out}");
        assert!(out.contains("listen 443 ssl;"), "{out}");
    }
}

#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::certs::CertStore;
    use std::sync::{Arc, Mutex};

    fn unique_dir() -> PathBuf {
        let n = Box::into_raw(Box::new(0u8)) as usize;
        std::env::temp_dir().join(format!("hoster-nginx-store-{n}"))
    }

    fn temp_conf() -> PathBuf {
        let n = Box::into_raw(Box::new(0u8)) as usize;
        std::env::temp_dir().join(format!("hoster-mgr-{n}.conf"))
    }

    #[test]
    fn bases_for_includes_only_domains_with_a_cert_on_disk() {
        let dir = unique_dir();
        let store = CertStore::new(dir.clone());
        // Give "*.dev.example.com" a cert; leave "*.team.example.com" without one.
        store.save("*.dev.example.com", "CHAIN", "KEY").unwrap();

        let bases = bases_for(
            &["*.dev.example.com".to_string(), "*.team.example.com".to_string()],
            &store,
        );
        assert_eq!(bases.len(), 1);
        assert_eq!(bases[0].server_name, ".dev.example.com");
        assert!(bases[0].cert_path.ends_with("cert.pem"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apply_now_renders_and_records_status() {
        let dir = unique_dir();
        let store = Arc::new(CertStore::new(dir.clone()));
        store.save("*.dev.example.com", "CHAIN", "KEY").unwrap();

        let conf = temp_conf();
        let backend = NginxBackend::with_runner(
            conf.clone(),
            vec!["true".into()],
            Box::new(|_args: &[&str]| {
                Ok(CmdOutput {
                    success: true,
                    stderr: String::new(),
                })
            }),
        );
        let status: NginxStatusHandle = Arc::new(Mutex::new(None));
        let mgr = NginxManager::new(
            backend,
            store.clone(),
            Box::new(|| vec!["*.dev.example.com".to_string()]),
            "127.0.0.1:8080".to_string(),
            status.clone(),
        );

        mgr.apply_now();

        let written = std::fs::read_to_string(&conf).expect("config written");
        assert!(written.contains("server_name .dev.example.com;"), "{written}");

        let rec = status.lock().unwrap().clone().expect("status recorded");
        assert!(rec.validated && rec.reloaded);

        std::fs::remove_dir_all(&dir).ok();
        let _ = std::fs::remove_file(&conf);
    }
}

#[cfg(test)]
mod apply_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn temp_conf() -> PathBuf {
        // A unique, non-existent path per test (no Date/rand available: use ptr).
        let n = Box::into_raw(Box::new(0u8)) as usize;
        std::env::temp_dir().join(format!("hoster-nginx-{n}.conf"))
    }

    /// A runner that records invoked argv and returns canned results keyed by
    /// the first arg ("nginx" for validate, anything else for reload).
    fn runner(validate_ok: bool, reload_ok: bool, calls: Arc<Mutex<Vec<String>>>) -> CommandRunner {
        Box::new(move |args: &[&str]| {
            calls.lock().unwrap().push(args.join(" "));
            let is_validate = args == ["nginx", "-t"];
            let ok = if is_validate { validate_ok } else { reload_ok };
            Ok(CmdOutput {
                success: ok,
                stderr: if ok { String::new() } else { "boom".into() },
            })
        })
    }

    #[test]
    fn happy_path_writes_validates_then_reloads() {
        let path = temp_conf();
        let calls = Arc::new(Mutex::new(vec![]));
        let be = NginxBackend::with_runner(
            path.clone(),
            vec!["systemctl".into(), "reload".into(), "nginx".into()],
            runner(true, true, calls.clone()),
        );
        let out = be.apply("CONFIG-A").unwrap();
        assert!(out.validated && out.reloaded);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "CONFIG-A");
        let c = calls.lock().unwrap();
        assert_eq!(c[0], "nginx -t");
        assert_eq!(c[1], "systemctl reload nginx");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_failure_restores_backup_and_does_not_reload() {
        let path = temp_conf();
        crate::certs::write_atomic(&path, b"GOOD", 0o644).unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let be = NginxBackend::with_runner(
            path.clone(),
            vec!["systemctl".into(), "reload".into(), "nginx".into()],
            runner(false, true, calls.clone()),
        );
        let out = be.apply("BAD").unwrap();
        assert!(!out.validated && !out.reloaded);
        assert_eq!(out.message.as_deref(), Some("boom"));
        // Last-good config is restored; no reload was attempted.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "GOOD");
        assert_eq!(*calls.lock().unwrap(), vec!["nginx -t".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reload_failure_is_surfaced_but_config_stays() {
        let path = temp_conf();
        let calls = Arc::new(Mutex::new(vec![]));
        let be = NginxBackend::with_runner(
            path.clone(),
            vec!["systemctl".into(), "reload".into(), "nginx".into()],
            runner(true, false, calls.clone()),
        );
        let out = be.apply("CONFIG-B").unwrap();
        assert!(out.validated && !out.reloaded);
        assert_eq!(out.message.as_deref(), Some("boom"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "CONFIG-B");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_spawn_error_restores_backup() {
        let path = temp_conf();
        crate::certs::write_atomic(&path, b"GOOD", 0o644).unwrap();
        let be = NginxBackend::with_runner(
            path.clone(),
            vec!["systemctl".into(), "reload".into(), "nginx".into()],
            Box::new(move |args: &[&str]| {
                if args == ["nginx", "-t"] {
                    anyhow::bail!("spawn fail")
                } else {
                    Ok(CmdOutput {
                        success: true,
                        stderr: String::new(),
                    })
                }
            }),
        );
        let result = be.apply("BAD");
        assert!(result.is_err());
        // Last-good config is restored, not left as the new unvalidated content.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "GOOD");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_failure_with_no_existing_file_removes_written_file() {
        let path = temp_conf();
        let calls = Arc::new(Mutex::new(vec![]));
        let be = NginxBackend::with_runner(
            path.clone(),
            vec!["systemctl".into(), "reload".into(), "nginx".into()],
            runner(false, true, calls.clone()),
        );
        let out = be.apply("BAD").unwrap();
        assert!(!out.validated && !out.reloaded);
        // No backup existed, so the unvalidated file is removed rather than restored.
        assert!(!path.exists());
        assert_eq!(*calls.lock().unwrap(), vec!["nginx -t".to_string()]);
        let _ = std::fs::remove_file(&path);
    }
}
