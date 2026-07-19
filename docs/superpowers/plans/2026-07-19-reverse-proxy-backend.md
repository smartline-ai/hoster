# Reverse-Proxy Backend (standalone vs. nginx) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in `nginx` proxy mode where nginx is the TLS-terminating edge that reverse-proxies all traffic to hoster's plain HTTP listener, while `standalone` (today's behavior, hoster owns `:80`/`:443`) stays the default.

**Architecture:** Option 1 from the spec. hoster keeps its wildcard ACME/DNS-01 issuance and its `Host`-based `RoutingTable` unchanged. In `nginx` mode it generates one nginx config file (`/etc/nginx/conf.d/hoster.conf`) with a shared `:80` block and one `:443` server block per wildcard base whose cert exists on disk, then validates with `nginx -t` and reloads. Config is (re)generated only at startup and on cert rotation — never per deploy, because branches flow through hoster's in-memory route hot-swap and nginx never sees them.

**Tech Stack:** Rust, tokio, hyper, `anyhow`. Reuses `certs::write_atomic`, `settings::wildcard_base`, `renewal::wanted_domains`, `CertStore::dir_for`. The nginx `nginx -t`/reload commands run behind an injected `CommandRunner` closure (the same swappable-seam idea as `Engine::with_dns_provider_builder`) so tests never shell out to real nginx.

## Global Constraints

- **Default is `standalone` = today's behavior, byte-for-byte.** With `HOSTER_PROXY_MODE` unset, nothing changes. All new fields are inert in standalone mode.
- **`nginx -t` before every reload is mandatory.** A failed validate must abort the reload and restore the last-good config file; never leave a broken file live and never reload an invalid config.
- **No per-deploy config regeneration.** `apply()` runs only at startup (nginx mode) and on cert rotation. Deploy/teardown never touch nginx.
- **Strict hostname validation.** Every `server_name` value written into the config must match `[a-z0-9.-]` label rules; anything else is skipped and logged (config-injection guard).
- **Secrets/output discipline.** No new secret is introduced, but keep the existing hand-written redacting `Debug` on `Settings` correct: the three new fields are non-secret and must appear in `Debug`.
- **Reuse `certs::write_atomic`** (it is `pub(crate)`) for all config-file writes; do not hand-roll file writes.
- **nginx directive form:** use `listen 443 ssl;` + `http2 on;` (nginx ≥ 1.25). Document the version floor in `docs/deploying.md`.

---

### Task 1: `ProxyMode` enum + nginx settings on `Settings`

**Files:**
- Modify: `src/settings.rs` (struct `Settings`, its `Debug` impl, add `ProxyMode`, add tests)
- Modify: `src/main.rs` (env parsing in the `Settings { .. }` literal, ~line 175-195)
- Modify (mechanical): every other `Settings { .. }` literal (test helpers) — found via grep

**Interfaces:**
- Produces:
  - `pub enum ProxyMode { Standalone, Nginx }` with `pub fn parse(s: &str) -> anyhow::Result<ProxyMode>` (case-insensitive; `"standalone"`→`Standalone`, `"nginx"`→`Nginx`, else error) and `#[derive(Clone, Copy, PartialEq, Eq, Debug)]`.
  - New `Settings` fields: `pub proxy_mode: ProxyMode`, `pub nginx_conf_path: String`, `pub nginx_reload_cmd: String`.

- [ ] **Step 1: Write the failing test** — append to the `#[cfg(test)] mod tests` in `src/settings.rs`:

```rust
    #[test]
    fn proxy_mode_parses_known_values_case_insensitively() {
        assert_eq!(ProxyMode::parse("standalone").unwrap(), ProxyMode::Standalone);
        assert_eq!(ProxyMode::parse("nginx").unwrap(), ProxyMode::Nginx);
        assert_eq!(ProxyMode::parse("NGINX").unwrap(), ProxyMode::Nginx);
    }

    #[test]
    fn proxy_mode_rejects_unknown_value_with_a_clear_message() {
        let err = ProxyMode::parse("caddy").unwrap_err().to_string();
        assert!(err.contains("caddy"), "message should name the bad value: {err}");
        assert!(err.contains("standalone") && err.contains("nginx"), "message should list valid values: {err}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p hoster --lib settings::tests::proxy_mode`
Expected: FAIL — `cannot find type ProxyMode`.

- [ ] **Step 3: Add the enum** — near the top of `src/settings.rs`, above `pub struct Settings`:

```rust
/// Which reverse-proxy topology hoster runs in.
///
/// `Standalone` is today's behavior: hoster binds `:80`/`:443` and is the edge
/// proxy. `Nginx` puts nginx in front as the TLS-terminating edge, proxying to
/// hoster's plain HTTP listener; hoster stops binding `:443` and instead
/// generates nginx config.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProxyMode {
    Standalone,
    Nginx,
}

impl ProxyMode {
    pub fn parse(s: &str) -> anyhow::Result<ProxyMode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" => Ok(ProxyMode::Standalone),
            "nginx" => Ok(ProxyMode::Nginx),
            other => anyhow::bail!(
                "unknown HOSTER_PROXY_MODE {other:?}; valid values are \"standalone\" and \"nginx\""
            ),
        }
    }
}
```

- [ ] **Step 4: Add the fields** — in `pub struct Settings`, after `public_ip`:

```rust
    /// Reverse-proxy topology. `Standalone` (default) = hoster is the edge.
    pub proxy_mode: ProxyMode,
    /// nginx mode only: the config file hoster generates.
    pub nginx_conf_path: String,
    /// nginx mode only: the shell command hoster runs to reload nginx after a
    /// successful `nginx -t`.
    pub nginx_reload_cmd: String,
```

- [ ] **Step 5: Extend the `Debug` impl** — in `impl std::fmt::Debug for Settings`, add three non-redacted fields before `.finish()`:

```rust
            .field("proxy_mode", &self.proxy_mode)
            .field("nginx_conf_path", &self.nginx_conf_path)
            .field("nginx_reload_cmd", &self.nginx_reload_cmd)
```

- [ ] **Step 6: Update every `Settings { .. }` literal.** Find them:

Run: `rg -n "Settings \{" src/`

For each literal (the `main.rs` production one is handled in Step 7; the rest are test helpers in `src/settings.rs`, `src/engine.rs`, `src/main.rs`, and any in `src/api.rs`/`src/ui/*`), add these three fields:

```rust
            proxy_mode: ProxyMode::Standalone,
            nginx_conf_path: "/etc/nginx/conf.d/hoster.conf".into(),
            nginx_reload_cmd: "systemctl reload nginx".into(),
```

Add `use crate::settings::ProxyMode;` (or `super::ProxyMode`) to any test module that now names it.

- [ ] **Step 7: Wire real env parsing in `main.rs`** — inside the `Arc::new(Settings { .. })` literal, after `public_ip: ...`, add:

```rust
        proxy_mode: ProxyMode::parse(&env_or("HOSTER_PROXY_MODE", "standalone"))?,
        nginx_conf_path: env_or("HOSTER_NGINX_CONF", "/etc/nginx/conf.d/hoster.conf"),
        nginx_reload_cmd: env_or("HOSTER_NGINX_RELOAD_CMD", "systemctl reload nginx"),
```

Add `ProxyMode` to the `use hoster::settings::...;` import at the top of `main.rs`.

- [ ] **Step 8: Run tests + build**

Run: `cargo test -p hoster --lib settings:: && cargo build`
Expected: PASS, and the whole crate compiles (all `Settings` literals fixed).

- [ ] **Step 9: Commit**

```bash
git add src/settings.rs src/main.rs src/engine.rs src/api.rs src/ui
git commit -m "feat(settings): add ProxyMode and nginx-mode settings"
```

---

### Task 2: `nginx::render` — pure config renderer

**Files:**
- Create: `src/nginx.rs`
- Modify: `src/lib.rs` (add `pub mod nginx;`)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct NginxBase { pub server_name: String, pub cert_path: std::path::PathBuf, pub key_path: std::path::PathBuf }`
  - `pub fn server_name_for(domain: &str) -> String` — `*.dev.example.com` → `.dev.example.com`; a plain name → itself.
  - `pub fn is_safe_server_name(name: &str) -> bool` — true iff every char is `[a-z0-9.-]` and it is non-empty.
  - `pub fn render(bases: &[NginxBase], upstream: &str) -> String`

- [ ] **Step 1: Write the failing tests** — create `src/nginx.rs`:

```rust
//! nginx-mode config generation. See docs/superpowers/specs/2026-07-19-reverse-proxy-backend-design.md.

use std::path::PathBuf;

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
```

- [ ] **Step 2: Register the module + run to verify it fails**

Add `pub mod nginx;` to `src/lib.rs` (alphabetical with the other `pub mod` lines).
Run: `cargo test -p hoster --lib nginx::render_tests`
Expected: FAIL — `render` / `NginxBase` not found.

- [ ] **Step 3: Implement the renderer** — add to `src/nginx.rs` (above the test module):

```rust
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hoster --lib nginx::render_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/nginx.rs src/lib.rs
git commit -m "feat(nginx): pure config renderer for nginx-mode server blocks"
```

---

### Task 3: `NginxBackend::apply` — write / validate / reload with a test seam

**Files:**
- Modify: `src/nginx.rs`

**Interfaces:**
- Consumes: `crate::certs::write_atomic` (`pub(crate)`).
- Produces:
  - `pub struct CmdOutput { pub success: bool, pub stderr: String }`
  - `pub type CommandRunner = Box<dyn Fn(&[&str]) -> anyhow::Result<CmdOutput> + Send + Sync>;`
  - `pub struct ApplyOutcome { pub validated: bool, pub reloaded: bool, pub message: Option<String> }`
  - `pub struct NginxBackend { .. }` with `pub fn new(conf_path: PathBuf, reload_cmd: Vec<String>) -> NginxBackend`, `#[cfg(test)] pub fn with_runner(conf_path, reload_cmd, runner) -> NginxBackend`, and `pub fn apply(&self, config: &str) -> anyhow::Result<ApplyOutcome>`.

- [ ] **Step 1: Write the failing tests** — add a second test module to `src/nginx.rs`:

```rust
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
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p hoster --lib nginx::apply_tests`
Expected: FAIL — `NginxBackend` not found.

- [ ] **Step 3: Implement** — add to `src/nginx.rs` (above the test modules):

```rust
use anyhow::Context;
use crate::certs::write_atomic;

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
        NginxBackend { conf_path, reload_cmd, runner: Box::new(real_runner) }
    }

    #[cfg(test)]
    pub fn with_runner(conf_path: PathBuf, reload_cmd: Vec<String>, runner: CommandRunner) -> NginxBackend {
        NginxBackend { conf_path, reload_cmd, runner }
    }

    /// Write `config`, validate with `nginx -t`, and reload on success.
    /// A failed validate restores the previous file and never reloads.
    pub fn apply(&self, config: &str) -> anyhow::Result<ApplyOutcome> {
        let backup = std::fs::read(&self.conf_path).ok();
        write_atomic(&self.conf_path, config.as_bytes(), 0o644)
            .with_context(|| format!("write {}", self.conf_path.display()))?;

        let validate = (self.runner)(&["nginx", "-t"])?;
        if !validate.success {
            match &backup {
                Some(bytes) => {
                    let _ = write_atomic(&self.conf_path, bytes, 0o644);
                }
                None => {
                    let _ = std::fs::remove_file(&self.conf_path);
                }
            }
            return Ok(ApplyOutcome { validated: false, reloaded: false, message: Some(validate.stderr) });
        }

        let reload_refs: Vec<&str> = self.reload_cmd.iter().map(String::as_str).collect();
        let reload = (self.runner)(&reload_refs)?;
        Ok(ApplyOutcome {
            validated: true,
            reloaded: reload.success,
            message: if reload.success { None } else { Some(reload.stderr) },
        })
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p hoster --lib nginx::apply_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/nginx.rs
git commit -m "feat(nginx): NginxBackend apply with validate-before-reload and test seam"
```

---

### Task 4: `NginxManager` — build bases, render, apply, record status

**Files:**
- Modify: `src/nginx.rs`

**Interfaces:**
- Consumes: `render`, `NginxBase`, `server_name_for`, `NginxBackend` (Task 2/3); `crate::certs::CertStore::dir_for`.
- Produces:
  - `pub struct ApplyRecord { pub validated: bool, pub reloaded: bool, pub message: Option<String>, pub at: i64 }`
  - `pub type NginxStatusHandle = std::sync::Arc<std::sync::Mutex<Option<ApplyRecord>>>;`
  - `pub fn bases_for(wanted: &[String], store: &crate::certs::CertStore) -> Vec<NginxBase>` — one base per wanted domain whose `dir_for(domain)/cert.pem` exists.
  - `pub struct NginxManager { .. }` with `pub fn new(backend, cert_store, wanted, upstream, status) -> NginxManager` and `pub fn apply_now(&self)`.

- [ ] **Step 1: Write the failing tests** — add a third test module to `src/nginx.rs`:

```rust
#[cfg(test)]
mod manager_tests {
    use super::*;
    use crate::certs::CertStore;
    use std::sync::{Arc, Mutex};

    fn unique_dir() -> PathBuf {
        let n = Box::into_raw(Box::new(0u8)) as usize;
        std::env::temp_dir().join(format!("hoster-nginx-store-{n}"))
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

        let conf = std::env::temp_dir().join(format!("hoster-mgr-{}.conf", dir.display().to_string().len()));
        let captured = Arc::new(Mutex::new(String::new()));
        let cap2 = captured.clone();
        let backend = NginxBackend::with_runner(
            conf.clone(),
            vec!["true".into()],
            Box::new(move |args: &[&str]| {
                if args != ["nginx", "-t"] { /* reload */ }
                *cap2.lock().unwrap() = std::fs::read_to_string(&conf_path_of(args)).unwrap_or_default();
                Ok(CmdOutput { success: true, stderr: String::new() })
            }),
        );
        // NOTE: conf_path_of is not real; capture via the file directly instead:
        let status: NginxStatusHandle = Arc::new(Mutex::new(None));
        let mgr = NginxManager::new(
            backend,
            store.clone(),
            Box::new(|| vec!["*.dev.example.com".to_string()]),
            "127.0.0.1:8080".to_string(),
            status.clone(),
        );
        mgr.apply_now();

        let rec = status.lock().unwrap().clone().expect("status recorded");
        assert!(rec.validated && rec.reloaded);
        let _ = captured; // (see below — assert on the written file instead)
        std::fs::remove_dir_all(&dir).ok();
        let _ = std::fs::remove_file(&conf);
    }
}
```

> Implementer note: the `apply_now` test above is intentionally simplified — drop the `captured`/`conf_path_of` scaffolding and instead assert on `std::fs::read_to_string(&conf)` after `apply_now()` (the file contains the rendered config, e.g. `server_name .dev.example.com;`). Keep the status assertion. Rewrite this test cleanly when implementing; the behavior under test is: `apply_now` writes rendered config for cert-bearing bases and records a validated+reloaded `ApplyRecord`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p hoster --lib nginx::manager_tests`
Expected: FAIL — `bases_for` / `NginxManager` not found.

- [ ] **Step 3: Implement** — add to `src/nginx.rs`:

```rust
use crate::certs::CertStore;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ApplyRecord {
    pub validated: bool,
    pub reloaded: bool,
    pub message: Option<String>,
    pub at: i64,
}

impl ApplyRecord {
    fn from_outcome(o: &ApplyOutcome, at: i64) -> ApplyRecord {
        ApplyRecord { validated: o.validated, reloaded: o.reloaded, message: o.message.clone(), at }
    }
    fn error(msg: String, at: i64) -> ApplyRecord {
        ApplyRecord { validated: false, reloaded: false, message: Some(msg), at }
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
        NginxManager { backend, cert_store, wanted, upstream, status }
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
```

- [ ] **Step 4: Run tests (with the cleaned-up test) to verify they pass**

Run: `cargo test -p hoster --lib nginx::manager_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/nginx.rs
git commit -m "feat(nginx): NginxManager builds bases from certs, applies, records status"
```

---

### Task 5: Cert-change hook on the renewal loop

**Files:**
- Modify: `src/renewal.rs` (`run_once`, `run_loop`, and existing `run_once` test call sites)

**Interfaces:**
- Consumes: nothing new.
- Produces:
  - `run_once(.., on_change: Option<&(dyn Fn() + Sync)>) -> BTreeMap<String, DomainState>` — calls `on_change` once per pass, after `rebuild`, iff a cert was issued this pass.
  - `run_loop(.., on_change: Option<Arc<dyn Fn() + Send + Sync>>)` — threads it through.

- [ ] **Step 1: Write the failing test** — add to `renewal`'s `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn run_once_calls_on_change_when_a_cert_is_issued() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        // Reuse whatever in-crate helper builds a store + issuer that succeeds
        // for one wanted domain. (See the existing run_once tests in this file
        // for the exact constructors; mirror them here.)
        let (issuer, cert_store, shared, wanted) = fixture_that_issues_one_cert();
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let on_change = move || { c.fetch_add(1, Ordering::SeqCst); };
        let state = run_once(
            issuer.as_ref(), &cert_store, &shared, &wanted,
            std::collections::BTreeMap::new(), now_secs(),
            Some(&on_change),
        ).await;
        assert!(state.is_empty(), "successful issuance clears state");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "on_change fires once on issuance");
    }
```

> Implementer note: `fixture_that_issues_one_cert()` is a stand-in — build the issuer/store/shared/wanted the same way the neighboring `run_once` tests in this file already do (grep `run_once(` in the test module for the exact setup and copy it). The assertion is what matters: `on_change` fires exactly once when a cert is saved.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p hoster --lib renewal::tests::run_once_calls_on_change`
Expected: FAIL — arity mismatch / helper missing.

- [ ] **Step 3: Thread the hook through `run_once`** — change its signature to add a final param and call it after `rebuild`:

```rust
pub async fn run_once(
    issuer: &dyn CertIssuer,
    store: &CertStore,
    shared: &SharedCerts,
    wanted: &[String],
    mut state: BTreeMap<String, DomainState>,
    now: i64,
    on_change: Option<&(dyn Fn() + Sync)>,
) -> BTreeMap<String, DomainState> {
    // ... unchanged body ...
    if changed {
        rebuild(store, shared, now);
        if let Some(f) = on_change {
            f();
        }
    }
    // ... unchanged tail (state.retain / return) ...
}
```

- [ ] **Step 4: Thread it through `run_loop`** — add the param and pass it down:

```rust
pub async fn run_loop(
    issuer: Arc<dyn CertIssuer>,
    store: Arc<CertStore>,
    shared: SharedCerts,
    wanted: impl Fn() -> Vec<String> + Send + 'static,
    trigger: RenewalTrigger,
    on_change: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    // ...
    // change the run_once call to:
    state = run_once(
        issuer.as_ref(), &store, &domains, /*..*/ state, now,
        on_change.as_deref().map(|f| f as &(dyn Fn() + Sync)),
    ).await;
    // ...
}
```

> Note: the existing call is `run_once(issuer.as_ref(), &store, &shared, &domains, state, now)`. Add `&shared` in its current position; only append the `on_change` argument. The `as_deref` maps `Option<Arc<..>>` → `Option<&dyn Fn>`.

- [ ] **Step 5: Fix existing `run_once` call sites** — every other `run_once(` in the test module needs a trailing `None`:

Run: `rg -n "run_once\(" src/renewal.rs`
For each existing test call, append `, None` before the closing `)`.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p hoster --lib renewal::`
Expected: PASS (new test + all pre-existing renewal tests).

- [ ] **Step 7: Commit**

```bash
git add src/renewal.rs
git commit -m "feat(renewal): fire an on_change hook when a cert is issued"
```

---

### Task 6: `main.rs` wiring — mode-conditional listeners, startup apply, renewal in nginx mode

**Files:**
- Modify: `src/main.rs` (the listener/TLS/renewal block, ~lines 205-300)

**Interfaces:**
- Consumes: `ProxyMode` (Task 1), `nginx::{NginxBackend, NginxManager, NginxStatusHandle}` (Tasks 3-4), the `run_loop` on_change param (Task 5), `Engine::with_nginx_status` (added here, see Step 3).

This task is integration glue; verification is `cargo build` + `cargo test` + the manual smoke in Step 6 (no new unit test — the units it composes are all tested in Tasks 1-5).

- [ ] **Step 1: Add the `Engine` status field** — in `src/engine.rs`, add a field, a builder, and a getter mirroring `with_renewal_trigger`:

In `pub struct Engine<R: ContainerRuntime>` add:
```rust
    nginx_status: Option<crate::nginx::NginxStatusHandle>,
```
In `Engine::new(..)` initializer add `nginx_status: None,`. Then add methods in the same `impl` block as `with_renewal_trigger`:
```rust
    pub fn with_nginx_status(mut self, h: crate::nginx::NginxStatusHandle) -> Self {
        self.nginx_status = Some(h);
        self
    }
    pub fn nginx_status(&self) -> Option<&crate::nginx::NginxStatusHandle> {
        self.nginx_status.as_ref()
    }
```
Update every `Engine { .. }` struct literal in `engine.rs` tests (grep `Engine {`) with `nginx_status: None,` if any construct the struct directly rather than via `Engine::new`.

- [ ] **Step 2: Compute the mode flags** — in `main.rs`, right after `let engine = Engine::new(..)` and before `let engine = Arc::new(if settings.https_listen.is_some() { .. })`, add:

```rust
    let nginx_mode = matches!(settings.proxy_mode, ProxyMode::Nginx);
    // hoster serves TLS itself only in standalone mode; nginx mode ignores
    // HOSTER_HTTPS_LISTEN and lets nginx own :443.
    let tls_serve = settings.https_listen.is_some() && !nginx_mode;
    // Certs are still issued in nginx mode — nginx serves them.
    let issue_certs = tls_serve || nginx_mode;
    if nginx_mode && settings.https_listen.is_some() {
        tracing::info!("nginx mode: ignoring HOSTER_HTTPS_LISTEN (nginx terminates TLS)");
    }
    let nginx_status: crate::nginx::NginxStatusHandle =
        std::sync::Arc::new(std::sync::Mutex::new(None));
```

- [ ] **Step 3: Attach the renewal trigger + nginx status to the engine** — replace the existing `let engine = Arc::new(if settings.https_listen.is_some() { engine.with_renewal_trigger(..) } else { engine });` with:

```rust
    let engine = {
        let mut e = engine;
        if issue_certs {
            e = e.with_renewal_trigger(renewal_trigger.clone());
        }
        if nginx_mode {
            e = e.with_nginx_status(nginx_status.clone());
        }
        Arc::new(e)
    };
```

- [ ] **Step 4: Gate the cert/TLS block on `issue_certs` and split serve-vs-issue** — replace the `if let Some(addr) = settings.https_listen.clone() { .. }` block with:

```rust
    let mut https: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    let mut renewal_task: Option<tokio::task::JoinHandle<()>> = None;
    if issue_certs {
        let cert_store = Arc::new(CertStore::new(PathBuf::from(&settings.cert_dir)));
        let now = renewal::now_secs();
        let shared = SharedCerts::new(CertResolver::from_certs(&cert_store.load_all(now))?);

        // hoster terminates TLS itself only in standalone mode.
        if tls_serve {
            let addr = settings.https_listen.clone().expect("tls_serve implies https_listen");
            let https_listener = TcpListener::bind(&addr)
                .await
                .with_context(|| format!("bind https {addr}"))?;
            https = Some(tokio::spawn(serve_https(
                https_listener,
                shared.clone(),
                routes.clone(),
                engine.clone(),
                settings.clone(),
                sessions.clone(),
            )));
        }

        let issuer: Arc<dyn CertIssuer> = Arc::new(StoreIssuer {
            store: store.clone(),
            settings: settings.clone(),
            account_path: PathBuf::from(env_or(
                "HOSTER_ACME_ACCOUNT_FILE",
                "/var/lib/hoster/acme-account.json",
            )),
            production: env_flag("HOSTER_ACME_PRODUCTION"),
        });

        let wanted_store = store.clone();
        let default_template = settings.hostname_template.clone();
        let wanted = move || renewal::wanted_domains(&wanted_store, &default_template);

        // nginx mode: build the manager, apply once now, and re-apply on rotation.
        let on_change: Option<Arc<dyn Fn() + Send + Sync>> = if nginx_mode {
            let backend = crate::nginx::NginxBackend::new(
                PathBuf::from(&settings.nginx_conf_path),
                settings.nginx_reload_cmd.split_whitespace().map(str::to_string).collect(),
            );
            let wanted_for_mgr = {
                let s = store.clone();
                let t = settings.hostname_template.clone();
                Box::new(move || renewal::wanted_domains(&s, &t)) as Box<dyn Fn() -> Vec<String> + Send + Sync>
            };
            let manager = Arc::new(crate::nginx::NginxManager::new(
                backend,
                cert_store.clone(),
                wanted_for_mgr,
                settings.listen.clone(),
                nginx_status.clone(),
            ));
            manager.apply_now(); // startup apply (non-fatal: records status, logs on failure)
            let mgr = manager.clone();
            Some(Arc::new(move || mgr.apply_now()))
        } else {
            None
        };

        renewal_task = Some(tokio::spawn(renewal::run_loop(
            issuer,
            cert_store,
            shared,
            wanted,
            renewal_trigger.clone(),
            on_change,
        )));
    }
```

- [ ] **Step 5: Build**

Run: `cargo build`
Expected: compiles. Fix any import lines (`ProxyMode`, `PathBuf`, `Arc` already imported).

- [ ] **Step 6: Manual smoke (no real nginx needed for standalone)**

Standalone unchanged:
```bash
HOSTER_TOKEN=x cargo run 2>&1 | head -5   # should log "hoster up"; ctrl-C
```
nginx mode, pointing at a temp conf + a harmless reload command so no real nginx is required:
```bash
HOSTER_TOKEN=x HOSTER_PROXY_MODE=nginx \
HOSTER_NGINX_CONF=/tmp/hoster-smoke.conf \
HOSTER_NGINX_RELOAD_CMD=true \
HOSTER_CERT_DIR=/tmp/hoster-smoke-certs \
cargo run 2>&1 | head -20
# Expect: no :443 bind, a nginx apply log line, and /tmp/hoster-smoke.conf written
cat /tmp/hoster-smoke.conf   # shows the :80 block (no :443 blocks until a cert exists)
```
Expected: standalone identical to before; nginx mode writes the conf file and does not bind `:443`.

- [ ] **Step 7: Commit**

```bash
git add src/main.rs src/engine.rs
git commit -m "feat(main): wire nginx proxy mode — listeners, startup apply, renewal, status"
```

---

### Task 7: Surface proxy mode + nginx status in the dashboard

**Files:**
- Modify: `src/ui/settings.rs` (`settings_body` + a new render helper)
- Modify: the settings-page handler in `src/api.rs` that calls `settings_body` (pass the status snapshot)

**Interfaces:**
- Consumes: `engine.nginx_status()` (Task 6), `settings.proxy_mode` / `nginx_conf_path` (Task 1), `ApplyRecord` (Task 4).
- Produces: a read-only "Proxy" section on the Settings page.

- [ ] **Step 1: Write the failing test** — add to `src/ui/settings.rs` tests (create the `#[cfg(test)] mod tests` if absent; mirror the file's existing test style):

```rust
    #[test]
    fn proxy_section_shows_mode_and_last_apply() {
        use crate::nginx::ApplyRecord;
        let mut body = String::new();
        render_proxy(
            &mut body,
            ProxyMode::Nginx,
            "/etc/nginx/conf.d/hoster.conf",
            Some(&ApplyRecord { validated: true, reloaded: true, message: None, at: 0 }),
        );
        assert!(body.contains("nginx"), "{body}");
        assert!(body.contains("/etc/nginx/conf.d/hoster.conf"), "{body}");
        assert!(body.contains("reloaded"), "{body}");
    }

    #[test]
    fn proxy_section_standalone_hides_nginx_details() {
        let mut body = String::new();
        render_proxy(&mut body, ProxyMode::Standalone, "/etc/nginx/conf.d/hoster.conf", None);
        assert!(body.contains("standalone"), "{body}");
        assert!(!body.contains("hoster.conf"), "no nginx path in standalone: {body}");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p hoster --lib ui::settings`
Expected: FAIL — `render_proxy` not found.

- [ ] **Step 3: Implement `render_proxy`** — add to `src/ui/settings.rs`. Use the file's existing escaping helper (grep for how `render_dns_row` escapes values — reuse the same `esc`/`html_escape` function; do not hand-roll):

```rust
/// The read-only Proxy section: proxy mode, and (nginx mode) the generated
/// config path plus the last apply result. Mode is env-set, so nothing here is
/// editable — it mirrors how the DNS panel surfaces state.
fn render_proxy(body: &mut String, mode: ProxyMode, conf_path: &str, last: Option<&crate::nginx::ApplyRecord>) {
    body.push_str("<section class=\"panel\"><div class=\"col\"><div class=\"col-label\">Proxy</div>");
    let mode_str = match mode {
        ProxyMode::Standalone => "standalone",
        ProxyMode::Nginx => "nginx",
    };
    body.push_str(&format!("<div class=\"env-row\"><span class=\"k\">Mode</span> <span class=\"tag\">{mode_str}</span></div>"));
    if mode == ProxyMode::Nginx {
        body.push_str(&format!(
            "<div class=\"env-row\"><span class=\"k\">Nginx config</span> <span class=\"v\">{}</span></div>",
            esc(conf_path) // reuse the file's existing escape helper
        ));
        match last {
            None => body.push_str(
                "<div class=\"env-row\"><span class=\"k\">Last apply</span> \
                 <span class=\"tag\">not yet applied</span></div>",
            ),
            Some(r) => {
                let (cls, label) = if r.validated && r.reloaded {
                    ("ok", "reloaded")
                } else if r.validated {
                    ("bad", "validated, reload failed")
                } else {
                    ("bad", "nginx -t failed")
                };
                body.push_str(&format!(
                    "<div class=\"env-row\"><span class=\"k\">Last apply</span> \
                     <span class=\"tag {cls}\">{label}</span></div>"
                ));
                if let Some(msg) = &r.message {
                    body.push_str(&format!(
                        "<div class=\"env-meta\"><pre>{}</pre></div>",
                        esc(msg)
                    ));
                }
            }
        }
    }
    body.push_str("</div></section>");
}
```

> Adjust `esc(..)` and the `tag ok`/`tag bad` class names to match whatever this file already uses (grep the file). The behavior — mode shown always, nginx path + last-apply shown only in nginx mode, `nginx -t` stderr escaped — is what the tests pin.

- [ ] **Step 4: Call `render_proxy` from `settings_body`** — add a `mode`/`conf`/`status` parameter path. Extend `settings_body`'s signature to accept `nginx_status: Option<&crate::nginx::ApplyRecord>` and call `render_proxy(&mut body, settings.proxy_mode, &settings.nginx_conf_path, nginx_status)` after the existing rows (e.g. after `row(&mut body, "API listen", ..)`). Update the handler in `src/api.rs` that renders the settings page to pass the snapshot:

```rust
    let nginx_snapshot = engine.nginx_status().and_then(|h| h.lock().unwrap().clone());
    // ... existing settings_body call gains: nginx_snapshot.as_ref()
```

Update any `settings_body(` call in tests to pass `None` for the new argument (grep `settings_body(`).

- [ ] **Step 5: Run tests + build**

Run: `cargo test -p hoster --lib ui::settings && cargo build`
Expected: PASS + compiles.

- [ ] **Step 6: Commit**

```bash
git add src/ui/settings.rs src/api.rs
git commit -m "feat(ui): read-only Proxy section with mode and last nginx apply result"
```

---

### Task 8: Operator docs for nginx mode

**Files:**
- Modify: `docs/deploying.md`

**Interfaces:** none (documentation).

- [ ] **Step 1: Add an "nginx proxy mode" section** to `docs/deploying.md` covering:
  - What the mode does (nginx is the edge; hoster proxied behind it; standalone is the default).
  - Env vars: `HOSTER_PROXY_MODE=nginx`, `HOSTER_NGINX_CONF` (default `/etc/nginx/conf.d/hoster.conf`), `HOSTER_NGINX_RELOAD_CMD` (default `systemctl reload nginx`). Note `HOSTER_HTTPS_LISTEN` is ignored in nginx mode and `HOSTER_LISTEN` is what nginx proxies to.
  - **Permissions:** hoster must be able to write `HOSTER_NGINX_CONF` and run `nginx -t` + the reload command — run hoster as root, or add a narrow sudoers entry. Show an example sudoers line, e.g.:
    ```
    hoster ALL=(root) NOPASSWD: /usr/sbin/nginx -t, /bin/systemctl reload nginx
    ```
  - **nginx version:** requires nginx ≥ 1.25 (uses `http2 on;`). Note the fallback (`listen 443 ssl http2;`) for older nginx if needed.
  - The lifecycle guarantee: config is generated at startup and on cert renewal, **not per deploy** — new branches need no nginx change because the wildcard cert + `Host` routing cover them.
  - Failure behavior: a failed `nginx -t` never reloads and restores the last-good file; check the dashboard's Proxy section for the last apply result and any `nginx -t` output.

- [ ] **Step 2: Commit**

```bash
git add docs/deploying.md
git commit -m "docs: document nginx proxy mode setup, permissions, and lifecycle"
```

---

## Self-Review

**Spec coverage:**
- Mode selection / settings (`HOSTER_PROXY_MODE`, `HOSTER_NGINX_CONF`, `HOSTER_NGINX_RELOAD_CMD`, default standalone) → Task 1. ✅
- Listener behavior (nginx mode binds only HTTP, drops `:443`, still issues certs) → Task 6 (`tls_serve`/`issue_certs`). ✅
- Pure renderer + one `:80` block + per-base `:443` blocks + omit certless bases + injection guard → Task 2 + Task 4 (`bases_for`). ✅
- Apply: atomic write, `nginx -t` before reload, restore-on-fail, captured stderr, test seam → Task 3. ✅
- Lifecycle: startup apply + rotation apply, never per deploy → Task 6 (startup) + Task 5 + Task 4 (`apply_now` from `on_change`). ✅
- Failure semantics: validate-before-reload mandatory, non-fatal startup, surfaced stderr → Task 3 + Task 6 (`apply_now` logs, non-fatal) + Task 7 (surface). ✅
- UI/API read-only status → Task 7. ✅
- Docs incl. permissions → Task 8. ✅
- Reuse ACME/DNS-01 unchanged → nothing modifies `acme.rs`/`dns.rs`; renewal only gains an optional hook. ✅

**Placeholder scan:** Tasks 4 and 5 contain deliberately-flagged test scaffolding (`conf_path_of`, `fixture_that_issues_one_cert`) with explicit implementer notes to rewrite them against the file's existing test constructors — the behavior under test is stated precisely. No `TBD`/`TODO` in production code steps.

**Type consistency:** `NginxBase`, `render(bases, upstream)`, `NginxBackend::{new,with_runner,apply}`, `ApplyOutcome`, `ApplyRecord`, `NginxStatusHandle`, `bases_for`, `NginxManager::{new,apply_now}`, `server_name_for`, `is_safe_server_name` are named identically across Tasks 2-7. `run_once`/`run_loop` gain a single trailing `on_change` param consistently (Task 5) and it is passed from Task 6. `Engine::{with_nginx_status,nginx_status}` defined in Task 6, consumed in Task 7. Consistent.
