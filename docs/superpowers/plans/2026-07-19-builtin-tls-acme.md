# Built-in TLS + ACME Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** hoster terminates TLS itself, obtaining and renewing Let's Encrypt wildcard certificates over DNS-01, so nginx is no longer needed.

**Architecture:** A `DnsProvider` trait (Cloudflare first) publishes `_acme-challenge` TXT records. An ACME module built on `instant-acme` drives issuance, waiting for real DNS propagation before asking Let's Encrypt to validate. Certificates persist to disk and are served through a rustls SNI resolver behind an `ArcSwap`, so a background renewal loop can hot-swap them without dropping connections.

**Tech Stack:** Rust, tokio, hyper, rustls 0.23, tokio-rustls 0.26, instant-acme 0.8, hickory-resolver 0.26, x509-parser 0.18, reqwest 0.12.

**Worktree:** `/Users/pavel/Projects/hoster-networking`, branch `networking`. All commands run from there.

**Prerequisite:** phases 1–3 of the spec, covered by `docs/superpowers/plans/2026-07-19-multi-domain-routing.md`. That plan must be executed first — this one consumes `Store::hostname_template_for` and the validated one-label template rule.

## Global Constraints

- The DNS API token is a **secret**: never returned by any read path, never logged, never rendered. Enforced by a separate masked type, not by remembering to omit a field. It can rewrite DNS, so treat it as more dangerous than the registry password.
- A certificate problem must never take a domain offline. A domain with no certificate keeps serving plain HTTP.
- `HOSTER_HTTPS_LISTEN` unset means no TLS listener and no issuance. Upgrading an existing install changes nothing.
- The on-disk store `version` stays at `1`; new fields are `Option` + `#[serde(default)]`. No migration.
- Never call a live ACME server or a live DNS API from the test suite.
- Rate limits are correctness, not polish: 5 authorization failures per identifier per hour, 5 duplicate certificates per identical name set per week, 50 certificates per registered domain per week counted globally. Backoff is mandatory.
- Clean under `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.

**Reference spec:** `docs/superpowers/specs/2026-07-19-builtin-tls-acme-design.md`

## ⚠️ Verify the `instant-acme` API before Task 4

I could not verify `instant-acme` 0.8.5's exact call signatures while writing this plan. Task 4's code is written against the shape documented in the crate's README (`Account::create`, `Account::new_order`, `order.authorizations()`, `challenge.key_authorization()`, `order.set_challenge_ready()`, `order.poll_ready()`, `order.finalize()`, `order.poll_certificate()`), but **names and argument types may differ**.

Task 4 Step 0 is therefore: read the actual API in `~/.cargo/registry/src/*/instant-acme-0.8.5/src/lib.rs` (or `cargo doc --open -p instant-acme`) and correct the task's code before writing it. Treat this plan's ACME snippets as structure, not gospel. Report any divergence in your task report so later tasks inherit the correction.

Everything else in this plan — the trait, cert store, SNI resolver, backoff, storage, API, dashboard — is written against APIs I verified.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/dns.rs` | Create | `DnsProvider` trait; `CloudflareProvider`; `FakeDns` for tests |
| `src/certs.rs` | Create | Certificate storage on disk, expiry parsing, renewal due-list |
| `src/acme.rs` | Create | Issuance: orders, challenges, propagation waiting, cleanup |
| `src/tls.rs` | Create | rustls config, SNI resolver with wildcard matching, hot swap |
| `src/renewal.rs` | Create | Background loop, per-domain backoff and failure state |
| `src/secrets.rs` | Modify | ACME config + DNS token storage, masked read path |
| `src/settings.rs` | Modify | `HOSTER_HTTPS_LISTEN`, `HOSTER_CERT_DIR`; wildcard-base derivation |
| `src/api.rs` | Modify | `/acme/*` endpoints; dashboard form routes |
| `src/dashboard.rs` | Modify | TLS & DNS section with per-domain certificate state |
| `src/main.rs` | Modify | Start the TLS listener and renewal loop |
| `README.md` | Modify | Document setup and the nginx cutover |
| `scripts/install.sh` | Modify | `AmbientCapabilities=CAP_NET_BIND_SERVICE` |

Eight tasks. Tasks 1–5 are independently testable library pieces with no wiring; Task 6 wires them into the server; Tasks 7–8 are UI and documentation.

---

### Task 1: Derive certificate domains from templates

**Files:**
- Modify: `src/settings.rs`

**Interfaces:**
- Produces: `pub fn wildcard_base(template: &str) -> Option<String>` — the wildcard a template's hostnames need (`{service}-{branch}.dev.example.com` → `*.dev.example.com`), or `None` if the template has no placeholder in its first label.
- Produces: `pub fn cert_identifiers(wildcard: &str) -> Vec<String>` — the identifier set for a certificate: the wildcard plus its bare parent (`*.dev.example.com` → `["*.dev.example.com", "dev.example.com"]`).

Including the bare parent is what produces two simultaneous TXT values at one `_acme-challenge` name, which is why `DnsProvider` must append rather than overwrite.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/settings.rs`:

```rust
    #[test]
    fn wildcard_base_replaces_the_first_label() {
        assert_eq!(
            wildcard_base("{service}-{branch}.dev.example.com").as_deref(),
            Some("*.dev.example.com")
        );
        assert_eq!(
            wildcard_base("{branch}.demo.example.com").as_deref(),
            Some("*.demo.example.com")
        );
    }

    #[test]
    fn wildcard_base_is_none_without_a_placeholder() {
        assert_eq!(wildcard_base("static.example.com"), None);
    }

    #[test]
    fn cert_identifiers_include_the_bare_parent() {
        assert_eq!(
            cert_identifiers("*.dev.example.com"),
            vec!["*.dev.example.com".to_string(), "dev.example.com".to_string()]
        );
    }

    #[test]
    fn cert_identifiers_of_a_plain_name_is_just_that_name() {
        assert_eq!(
            cert_identifiers("hoster.example.com"),
            vec!["hoster.example.com".to_string()]
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib settings`
Expected: FAIL to compile — `cannot find function wildcard_base`.

- [ ] **Step 3: Implement**

Add to `src/settings.rs`:

```rust
/// The wildcard certificate name covering every hostname a template produces.
/// Returns `None` when the first label has no placeholder, since such a
/// template yields one fixed hostname needing no wildcard.
pub fn wildcard_base(template: &str) -> Option<String> {
    let (first, rest) = template.split_once('.')?;
    if !first.contains('{') {
        return None;
    }
    Some(format!("*.{rest}"))
}

/// The identifier set for a certificate. A wildcard does not cover its own
/// parent, so `*.dev.example.com` is paired with `dev.example.com`.
pub fn cert_identifiers(name: &str) -> Vec<String> {
    match name.strip_prefix("*.") {
        Some(parent) => vec![name.to_string(), parent.to_string()],
        None => vec![name.to_string()],
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib settings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/settings.rs
git commit -m "feat: derive certificate names from hostname templates"
```

---

### Task 2: DNS provider trait and Cloudflare implementation

**Files:**
- Create: `src/dns.rs`
- Modify: `src/lib.rs`, `Cargo.toml`

**Interfaces:**
- Produces:
  - `pub trait DnsProvider` with `async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()>` and `async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()>`
  - `pub struct CloudflareProvider` with `pub fn new(token: String) -> Self`
  - `pub struct FakeDns` with `pub fn new() -> Self` and `pub fn values(&self, name: &str) -> Vec<String>`

**Contract, and the reason for each test:** `name` is fully qualified at the trait boundary. Cloudflare's API also uses fully-qualified names, so no conversion is needed here — but the conversion bug is the classic DNS-01 failure (a record that saves but never resolves), so the naming is asserted explicitly to protect the contract for the next provider. `upsert_txt` must **append** a value, leaving other values at that name intact, because a wildcard certificate validates two identifiers under one name simultaneously.

- [ ] **Step 1: Add dependencies**

```bash
cargo add reqwest --features json,rustls-tls --no-default-features
cargo add urlencoding
```

Run: `cargo check`
Expected: compiles.

- [ ] **Step 2: Write the failing tests**

Create `src/dns.rs`:

```rust
//! Publishing ACME DNS-01 challenge records.
//!
//! Names crossing this trait are **fully qualified**
//! (`_acme-challenge.dev.example.com`); each provider converts to whatever its
//! own API expects. Getting that wrong yields a record that appears to save but
//! never resolves — the most common DNS-01 failure.
//!
//! Both operations act on a single value and must leave other values at the
//! same name intact: a certificate for `*.dev.example.com` plus
//! `dev.example.com` publishes two TXT values under one name at once.

use async_trait::async_trait;

#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Publish `value` as a TXT record at `name`, keeping existing values.
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Remove exactly `value` at `name`, keeping other values.
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_appends_values_and_keeps_the_others() {
        let dns = FakeDns::new();
        dns.upsert_txt("_acme-challenge.dev.example.com", "one").await.unwrap();
        dns.upsert_txt("_acme-challenge.dev.example.com", "two").await.unwrap();
        let mut got = dns.values("_acme-challenge.dev.example.com");
        got.sort();
        assert_eq!(got, vec!["one".to_string(), "two".to_string()]);
    }

    #[tokio::test]
    async fn fake_deletes_only_the_named_value() {
        let dns = FakeDns::new();
        dns.upsert_txt("_acme-challenge.dev.example.com", "one").await.unwrap();
        dns.upsert_txt("_acme-challenge.dev.example.com", "two").await.unwrap();
        dns.delete_txt("_acme-challenge.dev.example.com", "one").await.unwrap();
        assert_eq!(dns.values("_acme-challenge.dev.example.com"), vec!["two".to_string()]);
    }

    #[tokio::test]
    async fn fake_delete_of_a_missing_value_is_not_an_error() {
        let dns = FakeDns::new();
        dns.delete_txt("_acme-challenge.dev.example.com", "nope").await.unwrap();
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Register the module first — add `pub mod dns;` to `src/lib.rs`.

Run: `cargo test --lib dns`
Expected: FAIL to compile — `cannot find type FakeDns`.

- [ ] **Step 4: Implement the fake**

Add to `src/dns.rs`, above the test module:

```rust
use std::collections::BTreeMap;
use std::sync::Mutex;

/// In-memory `DnsProvider` for tests.
#[derive(Default)]
pub struct FakeDns {
    records: Mutex<BTreeMap<String, Vec<String>>>,
}

impl FakeDns {
    pub fn new() -> Self {
        Self::default()
    }

    /// The TXT values currently published at `name` — for test assertions.
    pub fn values(&self, name: &str) -> Vec<String> {
        self.records.lock().unwrap().get(name).cloned().unwrap_or_default()
    }
}

#[async_trait]
impl DnsProvider for FakeDns {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let mut r = self.records.lock().unwrap();
        let entry = r.entry(name.to_string()).or_default();
        if !entry.iter().any(|v| v == value) {
            entry.push(value.to_string());
        }
        Ok(())
    }

    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let mut r = self.records.lock().unwrap();
        if let Some(entry) = r.get_mut(name) {
            entry.retain(|v| v != value);
        }
        Ok(())
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib dns`
Expected: PASS — 3 tests.

- [ ] **Step 6: Write the failing Cloudflare tests**

These run against a local mock HTTP server, never Cloudflare. Add to `src/dns.rs`'s test module:

```rust
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// A one-shot HTTP server returning canned JSON, recording the requests it
    /// received. Enough to assert request shape without touching the network.
    async fn mock_server(
        responses: Vec<(u16, String)>,
    ) -> (SocketAddr, std::sync::Arc<Mutex<Vec<(String, String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await.unwrap_or(0);
                let raw = String::from_utf8_lossy(&buf[..n]).to_string();
                let mut lines = raw.lines();
                let start = lines.next().unwrap_or("").to_string();
                let mut parts = start.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let body_in = raw.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                seen2.lock().unwrap().push((method, path, body_in));
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = tokio::io::AsyncWriteExt::write_all(&mut sock, resp.as_bytes()).await;
            }
        });
        (addr, seen)
    }

    #[tokio::test]
    async fn cloudflare_upsert_uses_the_fully_qualified_name() {
        let zones = r#"{"success":true,"result":[{"id":"zone123","name":"example.com"}]}"#;
        let created = r#"{"success":true,"result":{"id":"rec1"}}"#;
        let (addr, seen) = mock_server(vec![
            (200, zones.to_string()),
            (200, created.to_string()),
        ])
        .await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        cf.upsert_txt("_acme-challenge.dev.example.com", "val1").await.unwrap();

        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "expected a zone lookup then a record create");
        assert!(reqs[1].2.contains("_acme-challenge.dev.example.com"),
            "record must be created with the fully-qualified name, got: {}", reqs[1].2);
        assert!(reqs[1].2.contains("val1"));
        assert_eq!(reqs[1].0, "POST");
    }
```

- [ ] **Step 7: Run to verify it fails**

Run: `cargo test --lib dns::tests::cloudflare`
Expected: FAIL to compile — `cannot find CloudflareProvider`.

- [ ] **Step 8: Implement the Cloudflare provider**

Add to `src/dns.rs`:

```rust
use serde::Deserialize;

const CLOUDFLARE_API: &str = "https://api.cloudflare.com/client/v4";

/// Cloudflare DNS over the v4 API. Uses per-record CRUD by ID, so a write
/// touches exactly one value and cannot disturb unrelated records.
pub struct CloudflareProvider {
    token: String,
    base_url: String,
    http: reqwest::Client,
    zone_cache: Mutex<BTreeMap<String, String>>,
}

#[derive(Deserialize)]
struct CfList<T> {
    result: Vec<T>,
}

#[derive(Deserialize)]
struct CfZone {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct CfRecord {
    id: String,
    content: String,
}

impl CloudflareProvider {
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, CLOUDFLARE_API.to_string())
    }

    /// Construct against an arbitrary base URL, so tests can point at a local
    /// mock server instead of Cloudflare.
    pub fn with_base_url(token: String, base_url: String) -> Self {
        CloudflareProvider {
            token,
            base_url,
            http: reqwest::Client::new(),
            zone_cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// The zone owning `name`: the longest zone name that is a suffix of it.
    async fn zone_id(&self, name: &str) -> anyhow::Result<String> {
        if let Some(hit) = self.zone_cache.lock().unwrap().get(name).cloned() {
            return Ok(hit);
        }
        let url = format!("{}/zones", self.base_url);
        let resp: CfList<CfZone> = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let best = resp
            .result
            .into_iter()
            .filter(|z| name == z.name || name.ends_with(&format!(".{}", z.name)))
            .max_by_key(|z| z.name.len())
            .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone found for {name}"))?;
        self.zone_cache
            .lock()
            .unwrap()
            .insert(name.to_string(), best.id.clone());
        Ok(best.id)
    }

    async fn find_record(&self, zone: &str, name: &str, value: &str) -> anyhow::Result<Option<String>> {
        let url = format!(
            "{}/zones/{zone}/dns_records?type=TXT&name={}",
            self.base_url,
            urlencoding::encode(name)
        );
        let resp: CfList<CfRecord> = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp
            .result
            .into_iter()
            .find(|r| r.content.trim_matches('"') == value)
            .map(|r| r.id))
    }
}

#[async_trait]
impl DnsProvider for CloudflareProvider {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id(name).await?;
        // Creating a second TXT record at the same name adds a value; it does
        // not replace the first. That is required for wildcard + parent.
        let url = format!("{}/zones/{zone}/dns_records", self.base_url);
        self.http
            .post(url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "type": "TXT",
                "name": name,
                "content": value,
                "ttl": 60,
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id(name).await?;
        let Some(id) = self.find_record(&zone, name, value).await? else {
            return Ok(());
        };
        let url = format!("{}/zones/{zone}/dns_records/{id}", self.base_url);
        self.http
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
```

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test --lib dns`
Expected: PASS — 4 tests.

- [ ] **Step 10: Verify clean and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add src/dns.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: DNS provider trait with a Cloudflare implementation"
```

---

### Task 3: Certificate store

**Files:**
- Create: `src/certs.rs`
- Modify: `src/lib.rs`, `Cargo.toml`

**Interfaces:**
- Produces:
  - `pub struct CertStore` with `pub fn new(dir: PathBuf) -> Self`
  - `pub struct StoredCert { pub domain: String, pub chain_pem: String, pub key_pem: String, pub not_after: i64 }`
  - `CertStore::load_all(&self) -> Vec<StoredCert>`
  - `CertStore::save(&self, domain: &str, chain_pem: &str, key_pem: &str) -> anyhow::Result<()>`
  - `CertStore::due(&self, wanted: &[String], now: i64) -> Vec<String>` — domains from `wanted` with no certificate or expiring within 30 days

- [ ] **Step 1: Add the dependency**

```bash
cargo add x509-parser
```

- [ ] **Step 2: Write the failing tests**

Create `src/certs.rs` with the doc comment, a `todo!()`-bodied API, and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("hoster-certs-test-{}-{n}", std::process::id()))
    }

    /// A self-signed certificate valid for one hour, as PEM. Generated in-process
    /// so the test suite never needs a fixture file or a network call.
    fn self_signed(domain: &str, valid_secs: i64) -> (String, String, i64) {
        let mut params = rcgen::CertificateParams::new(vec![domain.to_string()]).unwrap();
        let now = std::time::SystemTime::now();
        params.not_before = now.into();
        params.not_after = (now + std::time::Duration::from_secs(valid_secs.max(1) as u64)).into();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let not_after = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64)
            + valid_secs;
        (cert.pem(), key.serialize_pem(), not_after)
    }

    #[test]
    fn save_then_load_round_trips() {
        let store = CertStore::new(temp_dir());
        let (chain, key, _) = self_signed("dev.example.com", 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        let all = store.load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].domain, "*.dev.example.com");
        assert!(all[0].chain_pem.contains("BEGIN CERTIFICATE"));
        assert!(all[0].key_pem.contains("PRIVATE KEY"));
    }

    #[test]
    fn stored_key_is_owner_only() {
        let store = CertStore::new(temp_dir());
        let (chain, key, _) = self_signed("dev.example.com", 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = store.dir_for("*.dev.example.com");
            let mode = std::fs::metadata(dir.join("key.pem")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
        }
    }

    #[test]
    fn due_includes_a_domain_with_no_certificate() {
        let store = CertStore::new(temp_dir());
        let due = store.due(&["*.dev.example.com".to_string()], 0);
        assert_eq!(due, vec!["*.dev.example.com".to_string()]);
    }

    #[test]
    fn due_excludes_a_certificate_with_plenty_of_life() {
        let store = CertStore::new(temp_dir());
        let (chain, key, not_after) = self_signed("dev.example.com", 90 * 24 * 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        let now = not_after - 90 * 24 * 3600;
        assert!(store.due(&["*.dev.example.com".to_string()], now).is_empty());
    }

    #[test]
    fn due_includes_a_certificate_inside_the_renewal_window() {
        let store = CertStore::new(temp_dir());
        let (chain, key, not_after) = self_signed("*.dev.example.com", 90 * 24 * 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        // 10 days before expiry — inside the 30-day window.
        let now = not_after - 10 * 24 * 3600;
        assert_eq!(
            store.due(&["*.dev.example.com".to_string()], now),
            vec!["*.dev.example.com".to_string()]
        );
    }

    #[test]
    fn an_unparseable_certificate_is_treated_as_absent_not_a_panic() {
        let store = CertStore::new(temp_dir());
        let dir = store.dir_for("*.dev.example.com");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fullchain.pem"), b"not a certificate").unwrap();
        std::fs::write(dir.join("key.pem"), b"not a key").unwrap();
        assert!(store.load_all().is_empty(), "corrupt files must not load");
        assert_eq!(
            store.due(&["*.dev.example.com".to_string()], 0),
            vec!["*.dev.example.com".to_string()],
            "a corrupt certificate should trigger reissuance"
        );
    }

    #[test]
    fn a_wildcard_domain_maps_to_a_filesystem_safe_directory() {
        let store = CertStore::new(temp_dir());
        let dir = store.dir_for("*.dev.example.com");
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.contains('*'), "'*' must not appear in a path: {name}");
    }
}
```

Add `rcgen` as a dev-dependency for the self-signed helper:

```bash
cargo add --dev rcgen
```

- [ ] **Step 3: Run to verify they fail**

Register the module — add `pub mod certs;` to `src/lib.rs`.

Run: `cargo test --lib certs`
Expected: FAIL — `not yet implemented` panics from the `todo!()` bodies.

- [ ] **Step 4: Implement**

Replace the `todo!()` bodies in `src/certs.rs`:

```rust
//! Certificates on disk.
//!
//! One directory per domain under the configured root, each holding
//! `fullchain.pem` and a `0600` `key.pem`. Certificates outlive restarts, so
//! hoster never reissues on boot when valid ones are already present.

use std::path::PathBuf;

use x509_parser::prelude::*;

/// Renew this long before expiry. Let's Encrypt certificates last 90 days.
const RENEW_WITHIN_SECS: i64 = 30 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCert {
    pub domain: String,
    pub chain_pem: String,
    pub key_pem: String,
    /// Expiry as a Unix timestamp.
    pub not_after: i64,
}

pub struct CertStore {
    dir: PathBuf,
}

impl CertStore {
    pub fn new(dir: PathBuf) -> Self {
        CertStore { dir }
    }

    /// The directory holding one domain's files. `*` is not usable in a path
    /// on every filesystem, so a wildcard is stored under `_wildcard.<rest>`.
    pub fn dir_for(&self, domain: &str) -> PathBuf {
        let safe = domain.replace('*', "_wildcard");
        self.dir.join(safe)
    }

    /// Every certificate currently on disk. Unreadable or unparseable entries
    /// are logged and skipped, so one corrupt file cannot stop startup.
    pub fn load_all(&self) -> Vec<StoredCert> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let (Ok(chain_pem), Ok(key_pem)) = (
                std::fs::read_to_string(dir.join("fullchain.pem")),
                std::fs::read_to_string(dir.join("key.pem")),
            ) else {
                continue;
            };
            let Some(not_after) = parse_not_after(&chain_pem) else {
                tracing::warn!(dir = %dir.display(), "unparseable certificate; ignoring");
                continue;
            };
            let name = entry.file_name().to_string_lossy().replace("_wildcard", "*");
            out.push(StoredCert {
                domain: name,
                chain_pem,
                key_pem,
                not_after,
            });
        }
        out
    }

    /// Write a certificate atomically, with the key owner-only.
    pub fn save(&self, domain: &str, chain_pem: &str, key_pem: &str) -> anyhow::Result<()> {
        let dir = self.dir_for(domain);
        std::fs::create_dir_all(&dir)?;
        write_atomic(&dir.join("fullchain.pem"), chain_pem.as_bytes(), 0o644)?;
        write_atomic(&dir.join("key.pem"), key_pem.as_bytes(), 0o600)?;
        Ok(())
    }

    /// Which of `wanted` need issuing: absent, unparseable, or within the
    /// renewal window.
    pub fn due(&self, wanted: &[String], now: i64) -> Vec<String> {
        let have = self.load_all();
        wanted
            .iter()
            .filter(|d| {
                match have.iter().find(|c| &&c.domain == d) {
                    None => true,
                    Some(c) => c.not_after - now <= RENEW_WITHIN_SECS,
                }
            })
            .cloned()
            .collect()
    }
}

/// Expiry of the first certificate in a PEM chain, or `None` if unparseable.
fn parse_not_after(pem: &str) -> Option<i64> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).ok()?;
    let cert = p.parse_x509().ok()?;
    Some(cert.validity().not_after.timestamp())
}

fn write_atomic(path: &std::path::Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib certs`
Expected: PASS — 7 tests.

- [ ] **Step 6: Verify clean and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add src/certs.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: certificate store with expiry-based renewal selection"
```

---

### Task 4: ACME issuance

**Files:**
- Create: `src/acme.rs`
- Modify: `src/lib.rs`, `Cargo.toml`

**Interfaces:**
- Consumes: `DnsProvider` (Task 2), `cert_identifiers` (Task 1).
- Produces:
  - `pub struct Issuer` with `pub fn new(account_credentials_path: PathBuf, email: String, dns: Arc<dyn DnsProvider>) -> Self`
  - `pub struct IssuedCert { pub chain_pem: String, pub key_pem: String }`
  - `Issuer::issue(&self, domain: &str) -> anyhow::Result<IssuedCert>`
  - `pub async fn wait_for_txt(resolver: &TokioResolver, name: &str, expected: &str, timeout: Duration) -> anyhow::Result<()>`

- [ ] **Step 0: Verify the instant-acme API — do this first**

Read the real API before writing any code:

```bash
ls ~/.cargo/registry/src/*/instant-acme-0.8.5/src/
sed -n '1,120p' ~/.cargo/registry/src/*/instant-acme-0.8.5/src/lib.rs
```

The code below is written against the crate's documented shape, but its exact names and argument types were **not verified** when this plan was written. Correct the code to match the real API, and record every divergence in your task report so later tasks inherit it. If the API differs so much that the structure below no longer fits, stop and report rather than improvising a large redesign.

- [ ] **Step 1: Add dependencies**

```bash
cargo add instant-acme
cargo add hickory-resolver
```

- [ ] **Step 2: Write the failing propagation test**

The propagation wait is the part that is both testable without a network and the most common source of flakiness, so it is tested directly. Create `src/acme.rs` with this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_name_is_prefixed_and_stripped_of_the_wildcard() {
        assert_eq!(challenge_name("*.dev.example.com"), "_acme-challenge.dev.example.com");
        assert_eq!(challenge_name("dev.example.com"), "_acme-challenge.dev.example.com");
        assert_eq!(challenge_name("hoster.example.com"), "_acme-challenge.hoster.example.com");
    }

    #[test]
    fn wildcard_and_parent_share_one_challenge_name() {
        // This is why the DNS provider must append rather than overwrite.
        assert_eq!(
            challenge_name("*.dev.example.com"),
            challenge_name("dev.example.com")
        );
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Add `pub mod acme;` to `src/lib.rs`.

Run: `cargo test --lib acme`
Expected: FAIL to compile — `cannot find function challenge_name`.

- [ ] **Step 4: Implement the challenge-name helper**

Add to `src/acme.rs`:

```rust
//! Obtaining certificates from Let's Encrypt over DNS-01.
//!
//! The challenge record for `*.dev.example.com` and for `dev.example.com` is
//! the same name, so a certificate covering both publishes two values there at
//! once — the DNS provider must append, never replace.

/// The TXT record name a domain's DNS-01 challenge is published at.
pub fn challenge_name(domain: &str) -> String {
    let bare = domain.strip_prefix("*.").unwrap_or(domain);
    format!("_acme-challenge.{bare}")
}
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test --lib acme`
Expected: PASS — 2 tests.

- [ ] **Step 6: Implement propagation waiting**

Waiting for the record to be visible before asking Let's Encrypt to validate is what keeps issuance off the 5-failures-per-identifier-per-hour limit. Query the authoritative nameservers directly rather than sleeping.

```rust
use std::time::Duration;

use hickory_resolver::TokioResolver;

/// Poll DNS until `expected` appears among the TXT values at `name`.
///
/// A fixed sleep is not enough: propagation time varies by provider and by
/// record, and every premature validation attempt burns one of the five
/// authorization failures Let's Encrypt allows per identifier per hour.
pub async fn wait_for_txt(
    resolver: &TokioResolver,
    name: &str,
    expected: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delay = Duration::from_secs(2);
    loop {
        if let Ok(lookup) = resolver.txt_lookup(name).await {
            let found = lookup.iter().any(|txt| {
                txt.iter()
                    .any(|chunk| String::from_utf8_lossy(chunk) == expected)
            });
            if found {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for TXT {name} to publish the challenge value");
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(15));
    }
}
```

- [ ] **Step 7: Implement the issuer**

Write `Issuer` against the API you verified in Step 0. The required behaviour, regardless of exact call names:

1. Load persisted account credentials from `account_credentials_path` if present; otherwise create an account with `email`, and persist the returned credentials as JSON at `0600`. Never register a new account when a persisted one exists.
2. Create an order for `cert_identifiers(domain)`.
3. For each authorization, take the DNS-01 challenge, compute the key authorization digest, and publish it via `self.dns.upsert_txt(&challenge_name(&identifier), &digest)`.
4. Call `wait_for_txt` for every published value before marking any challenge ready.
5. Mark challenges ready, poll the order, finalize with a freshly generated key, and poll for the certificate chain.
6. **Always** clean up published TXT records with `delete_txt`, on both the success and the error path. Use a guard or an explicit `match` — a `?` that skips cleanup leaves stale records that break the next issuance.
7. Return `IssuedCert { chain_pem, key_pem }`.

Structure it so the cleanup cannot be skipped: gather the published `(name, value)` pairs into a `Vec`, run the issuance in an inner async block, then clean up unconditionally before returning the inner result.

- [ ] **Step 8: Verify it compiles and the unit tests pass**

Run: `cargo test --lib acme`
Expected: PASS. There is no live-server test; issuance is exercised end-to-end manually in the final verification.

- [ ] **Step 9: Verify clean and commit**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add src/acme.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: ACME DNS-01 issuance with real propagation waiting"
```

---

### Task 5: TLS listener and SNI resolution

**Files:**
- Create: `src/tls.rs`
- Modify: `src/lib.rs`, `Cargo.toml`

**Interfaces:**
- Consumes: `StoredCert` (Task 3).
- Produces:
  - `pub struct CertResolver` with `pub fn from_certs(certs: &[StoredCert]) -> anyhow::Result<Self>`
  - `pub struct SharedCerts` with `pub fn new(resolver: CertResolver) -> Self`, `pub fn swap(&self, resolver: CertResolver)`, `pub fn server_config(&self) -> Arc<ServerConfig>`
  - `pub fn matches(cert_domain: &str, sni: &str) -> bool`

Wildcard matching is the security-relevant part: `*.dev.example.com` must match `backend-main.dev.example.com` but **not** `a.b.dev.example.com` and **not** the bare `dev.example.com`.

- [ ] **Step 1: Add dependencies**

```bash
cargo add rustls --no-default-features --features ring,std,tls12
cargo add tokio-rustls --no-default-features --features ring,tls12
cargo add rustls-pemfile
```

- [ ] **Step 2: Write the failing tests**

Create `src/tls.rs` with this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_matches_exactly_one_label() {
        assert!(matches("*.dev.example.com", "backend-main.dev.example.com"));
        assert!(matches("*.dev.example.com", "anything.dev.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_two_labels() {
        assert!(!matches("*.dev.example.com", "a.b.dev.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_its_own_parent() {
        assert!(!matches("*.dev.example.com", "dev.example.com"));
    }

    #[test]
    fn wildcard_does_not_match_a_different_suffix() {
        assert!(!matches("*.dev.example.com", "backend.demo.example.com"));
        assert!(!matches("*.dev.example.com", "backend.dev.example.com.evil.com"));
    }

    #[test]
    fn exact_name_matches_only_itself() {
        assert!(matches("hoster.example.com", "hoster.example.com"));
        assert!(!matches("hoster.example.com", "other.example.com"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(matches("*.dev.example.com", "Backend-Main.DEV.example.com"));
    }
}
```

- [ ] **Step 3: Run to verify they fail**

Add `pub mod tls;` to `src/lib.rs`.

Run: `cargo test --lib tls`
Expected: FAIL to compile — `cannot find function matches`.

- [ ] **Step 4: Implement matching**

```rust
//! TLS termination with per-domain certificates selected by SNI.

/// Does a certificate's domain cover this SNI name?
///
/// A wildcard covers exactly one label: `*.dev.example.com` matches
/// `backend.dev.example.com` but neither `a.b.dev.example.com` nor the bare
/// `dev.example.com`, which needs its own identifier on the certificate.
pub fn matches(cert_domain: &str, sni: &str) -> bool {
    let cert = cert_domain.to_ascii_lowercase();
    let sni = sni.to_ascii_lowercase();
    match cert.strip_prefix("*.") {
        None => cert == sni,
        Some(parent) => match sni.strip_suffix(parent) {
            // The remaining prefix must be exactly one non-empty label plus its dot.
            Some(prefix) => {
                prefix.ends_with('.') && !prefix.is_empty() && !prefix[..prefix.len() - 1].contains('.')
            }
            None => false,
        },
    }
}
```

- [ ] **Step 5: Run to verify they pass**

Run: `cargo test --lib tls`
Expected: PASS — 6 tests.

- [ ] **Step 6: Implement the resolver and shared config**

```rust
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::ServerConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::certs::StoredCert;

/// Certificates indexed by the domain they were issued for, selected by SNI.
#[derive(Debug)]
pub struct CertResolver {
    entries: Vec<(String, Arc<CertifiedKey>)>,
}

impl CertResolver {
    /// Build from stored certificates. Entries that fail to parse are skipped
    /// with a warning rather than failing the whole resolver — one bad
    /// certificate must not remove TLS for every other domain.
    pub fn from_certs(certs: &[StoredCert]) -> anyhow::Result<Self> {
        let provider = rustls::crypto::ring::default_provider();
        let mut entries = Vec::new();
        for c in certs {
            match build_certified_key(&provider, &c.chain_pem, &c.key_pem) {
                Ok(ck) => entries.push((c.domain.to_ascii_lowercase(), Arc::new(ck))),
                Err(e) => tracing::warn!(domain = %c.domain, error = %e, "skipping unusable certificate"),
            }
        }
        Ok(CertResolver { entries })
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = hello.server_name()?;
        self.entries
            .iter()
            .find(|(domain, _)| matches(domain, sni))
            .map(|(_, ck)| ck.clone())
    }
}

fn build_certified_key(
    provider: &rustls::crypto::CryptoProvider,
    chain_pem: &str,
    key_pem: &str,
) -> anyhow::Result<CertifiedKey> {
    let chain: Vec<_> = rustls_pemfile::certs(&mut chain_pem.as_bytes())
        .collect::<Result<_, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("no private key in PEM"))?;
    let signing_key = provider.key_provider.load_private_key(key)?;
    Ok(CertifiedKey::new(chain, signing_key))
}

/// The live TLS config, swappable without dropping connections — the same
/// pattern `SharedRoutes` uses for the routing table.
pub struct SharedCerts(ArcSwap<ServerConfig>);

impl SharedCerts {
    pub fn new(resolver: CertResolver) -> Self {
        SharedCerts(ArcSwap::from_pointee(config_from(resolver)))
    }

    pub fn swap(&self, resolver: CertResolver) {
        self.0.store(Arc::new(config_from(resolver)));
    }

    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.0.load_full()
    }
}

fn config_from(resolver: CertResolver) -> ServerConfig {
    ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver))
}
```

- [ ] **Step 7: Add a resolver test**

```rust
    #[test]
    fn resolver_skips_an_unusable_certificate_without_failing() {
        let bad = StoredCert {
            domain: "*.dev.example.com".into(),
            chain_pem: "not a cert".into(),
            key_pem: "not a key".into(),
            not_after: i64::MAX,
        };
        let r = CertResolver::from_certs(&[bad]).unwrap();
        assert!(r.entries.is_empty(), "unusable certificate should be skipped, not fatal");
    }
```

- [ ] **Step 8: Run the tests, verify clean, commit**

Run: `cargo test --lib tls && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add src/tls.rs src/lib.rs Cargo.toml Cargo.lock
git commit -m "feat: SNI certificate resolution with hot-swappable TLS config"
```

---

### Task 6: Configuration storage and the renewal loop

**Files:**
- Create: `src/renewal.rs`
- Modify: `src/secrets.rs`, `src/settings.rs`, `src/main.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–5.
- Produces:
  - `Store::set_acme_config(&self, email: &str, control_hostname: Option<&str>) -> Result<(), String>`
  - `Store::set_dns_token(&self, kind: &str, token: &str) -> Result<(), String>`
  - `Store::delete_dns_token(&self) -> anyhow::Result<()>`
  - `Store::acme_config(&self) -> Option<AcmeConfig>` (includes the token; internal use only)
  - `Store::masked_acme(&self) -> Option<MaskedAcme>` — `{ email, control_hostname, provider_kind, token_set: bool }`, **never** the token
  - `pub fn next_attempt(failures: u32, last_attempt: i64) -> i64` (free function in `src/renewal.rs`)
  - `Settings` gains `https_listen: Option<String>`, `cert_dir: String`

- [ ] **Step 1: Write the failing backoff and storage tests**

Create `src/renewal.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_at_fifteen_minutes() {
        assert_eq!(next_attempt(1, 1000), 1000 + 15 * 60);
    }

    #[test]
    fn backoff_doubles_each_failure() {
        assert_eq!(next_attempt(2, 0), 30 * 60);
        assert_eq!(next_attempt(3, 0), 60 * 60);
    }

    #[test]
    fn backoff_caps_at_twenty_four_hours() {
        // Without a cap this would exceed a day and keep growing.
        assert_eq!(next_attempt(20, 0), 24 * 3600);
    }

    #[test]
    fn no_failures_means_try_immediately() {
        assert_eq!(next_attempt(0, 5000), 5000);
    }
}
```

Add to `src/secrets.rs`'s tests:

```rust
    #[test]
    fn masked_acme_never_exposes_the_dns_token() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", Some("hoster.example.com")).unwrap();
        s.set_dns_token("cloudflare", "cf_topsecret_token").unwrap();
        let masked = s.masked_acme().unwrap();
        let json = serde_json::to_string(&masked).unwrap();
        assert!(!json.contains("cf_topsecret_token"), "token leaked: {json}");
        assert!(json.contains("me@example.com"));
        assert!(masked.token_set);
    }

    #[test]
    fn acme_config_round_trips_including_the_token() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", None).unwrap();
        s.set_dns_token("cloudflare", "tok").unwrap();
        let cfg = s.acme_config().unwrap();
        assert_eq!(cfg.email, "me@example.com");
        assert_eq!(cfg.provider.token, "tok");
    }

    #[test]
    fn delete_dns_token_keeps_the_email() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", None).unwrap();
        s.set_dns_token("cloudflare", "tok").unwrap();
        s.delete_dns_token().unwrap();
        let masked = s.masked_acme().unwrap();
        assert_eq!(masked.email, "me@example.com");
        assert!(!masked.token_set);
    }

    #[test]
    fn set_acme_config_rejects_an_email_without_an_at_sign() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_acme_config("not-an-email", None).is_err());
    }

    #[test]
    fn set_dns_token_rejects_an_unknown_provider_kind() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_dns_token("bind9", "tok").is_err());
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib renewal secrets`
Expected: FAIL to compile.

- [ ] **Step 3: Implement backoff**

In `src/renewal.rs`:

```rust
//! The background certificate renewal loop.

const BASE_BACKOFF_SECS: i64 = 15 * 60;
const MAX_BACKOFF_SECS: i64 = 24 * 3600;

/// When a domain may next be attempted, given how many times it has failed.
///
/// Backoff is a correctness requirement, not politeness: Let's Encrypt permits
/// five authorization failures per identifier per hour and five duplicate
/// certificates per identical name set per week. A tight retry loop locks the
/// domain out for a week.
pub fn next_attempt(failures: u32, last_attempt: i64) -> i64 {
    if failures == 0 {
        return last_attempt;
    }
    let shift = (failures - 1).min(20);
    let delay = BASE_BACKOFF_SECS
        .saturating_mul(1i64.checked_shl(shift).unwrap_or(i64::MAX))
        .min(MAX_BACKOFF_SECS);
    last_attempt + delay
}
```

- [ ] **Step 4: Implement ACME storage**

In `src/secrets.rs`, add the types and store methods. `AcmeConfig` carries the token; `MaskedAcme` structurally cannot:

```rust
/// A DNS provider's credentials. The token can rewrite DNS — treat it as the
/// most dangerous secret in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsProviderConfig {
    pub kind: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeConfig {
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<DnsProviderConfig>,
}

/// ACME configuration as exposed to the UI/API: **never** the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedAcme {
    pub email: String,
    pub control_hostname: Option<String>,
    pub provider_kind: Option<String>,
    pub token_set: bool,
}
```

Add `acme: Option<AcmeConfig>` to `Data` with `#[serde(default)]`, then the methods:

```rust
    /// Set the ACME account email and optional control hostname, keeping any
    /// stored DNS credentials.
    pub fn set_acme_config(&self, email: &str, control_hostname: Option<&str>) -> Result<(), String> {
        if !email.contains('@') || email.len() < 3 {
            return Err(format!("{email:?} is not a valid email address"));
        }
        if let Some(h) = control_hostname {
            crate::settings::validate_hostname(h)?;
        }
        let mut data = self.data.lock().unwrap();
        match data.acme.as_mut() {
            Some(a) => {
                a.email = email.to_string();
                a.control_hostname = control_hostname.map(str::to_string);
            }
            None => {
                data.acme = Some(AcmeConfig {
                    email: email.to_string(),
                    control_hostname: control_hostname.map(str::to_string),
                    provider: None,
                })
            }
        }
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Set the DNS provider credentials. Requires the ACME email to be set
    /// first, since issuance needs both.
    pub fn set_dns_token(&self, kind: &str, token: &str) -> Result<(), String> {
        if kind != "cloudflare" {
            return Err(format!(
                "unknown DNS provider {kind:?}; supported providers: cloudflare"
            ));
        }
        if token.trim().is_empty() {
            return Err("DNS API token must not be empty".to_string());
        }
        if token.len() > MAX_VALUE_LEN {
            return Err(format!("token too long (max {MAX_VALUE_LEN} bytes)"));
        }
        let mut data = self.data.lock().unwrap();
        let Some(acme) = data.acme.as_mut() else {
            return Err("set the ACME account email before adding DNS credentials".to_string());
        };
        acme.provider = Some(DnsProviderConfig {
            kind: kind.to_string(),
            token: token.to_string(),
        });
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Remove the DNS credentials, keeping the rest of the ACME config.
    pub fn delete_dns_token(&self) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(a) = data.acme.as_mut() {
            a.provider = None;
        }
        self.persist(&data)
    }

    /// Full ACME config including the token — for issuance only, never a read path.
    pub fn acme_config(&self) -> Option<AcmeConfig> {
        self.data.lock().unwrap().acme.clone()
    }

    /// ACME config for display. Structurally cannot carry the token.
    pub fn masked_acme(&self) -> Option<MaskedAcme> {
        self.data.lock().unwrap().acme.as_ref().map(|a| MaskedAcme {
            email: a.email.clone(),
            control_hostname: a.control_hostname.clone(),
            provider_kind: a.provider.as_ref().map(|p| p.kind.clone()),
            token_set: a.provider.is_some(),
        })
    }
```

Note `acme_config()` returns the token — one of the two tests above asserts exactly this, and it is the only path allowed to. Add `pub fn validate_hostname(name: &str) -> Result<(), String>` to `src/settings.rs` by extracting the existing `validate_dns_name` body and making it public.

- [ ] **Step 5: Add the settings fields**

In `src/settings.rs` add to `Settings`:

```rust
    pub https_listen: Option<String>,
    pub cert_dir: String,
```

In `src/main.rs`:

```rust
        https_listen: std::env::var("HOSTER_HTTPS_LISTEN").ok().filter(|s| !s.is_empty()),
        cert_dir: env_or("HOSTER_CERT_DIR", "/var/lib/hoster/certs"),
```

Update every `Settings { .. }` literal in tests to include both fields — `grep -rn "hostname_template:" src/` finds them.

- [ ] **Step 6: Implement the renewal loop**

Add to `src/renewal.rs`. It owns failure state per domain in memory; state is rebuilt on restart, which is acceptable because the certificate store is the durable record.

```rust
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::acme::Issuer;
use crate::certs::CertStore;
use crate::tls::{CertResolver, SharedCerts};

/// Per-domain failure state, for backoff and for display.
#[derive(Debug, Clone, Default)]
pub struct DomainState {
    pub failures: u32,
    pub last_attempt: i64,
    pub last_error: Option<String>,
}

/// Run one renewal pass: issue every domain that is due and not in backoff.
/// Returns the updated failure state.
pub async fn run_once(
    issuer: &Issuer,
    store: &CertStore,
    shared: &SharedCerts,
    wanted: &[String],
    mut state: BTreeMap<String, DomainState>,
    now: i64,
) -> BTreeMap<String, DomainState> {
    let mut changed = false;
    for domain in store.due(wanted, now) {
        let st = state.entry(domain.clone()).or_default();
        if now < next_attempt(st.failures, st.last_attempt) {
            continue;
        }
        st.last_attempt = now;
        match issuer.issue(&domain).await {
            Ok(cert) => match store.save(&domain, &cert.chain_pem, &cert.key_pem) {
                Ok(()) => {
                    tracing::info!(domain = %domain, "certificate issued");
                    st.failures = 0;
                    st.last_error = None;
                    changed = true;
                }
                Err(e) => {
                    tracing::error!(domain = %domain, error = %e, "failed to save certificate");
                    st.failures += 1;
                    st.last_error = Some(e.to_string());
                }
            },
            Err(e) => {
                tracing::warn!(domain = %domain, error = %e, "certificate issuance failed");
                st.failures += 1;
                st.last_error = Some(e.to_string());
            }
        }
    }
    if changed {
        match CertResolver::from_certs(&store.load_all()) {
            Ok(r) => shared.swap(r),
            Err(e) => tracing::error!(error = %e, "failed to rebuild the certificate resolver"),
        }
    }
    state
}

/// Every 6 hours, run a renewal pass.
pub async fn run_loop(
    issuer: Arc<Issuer>,
    store: Arc<CertStore>,
    shared: Arc<SharedCerts>,
    wanted: impl Fn() -> Vec<String> + Send + 'static,
) {
    let mut state = BTreeMap::new();
    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        state = run_once(&issuer, &store, &shared, &wanted(), state, now).await;
        tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
    }
}
```

- [ ] **Step 7: Add a loop test using a fake issuer**

Make `Issuer::issue` reachable behind a trait so the loop is testable without ACME. Add to `src/acme.rs`:

```rust
#[async_trait::async_trait]
pub trait CertIssuer: Send + Sync {
    async fn issue(&self, domain: &str) -> anyhow::Result<IssuedCert>;
}
```

Implement it for `Issuer`, change `run_once`/`run_loop` to take `&dyn CertIssuer`, and add to `src/renewal.rs`'s tests:

```rust
    struct AlwaysFails;

    #[async_trait::async_trait]
    impl crate::acme::CertIssuer for AlwaysFails {
        async fn issue(&self, _domain: &str) -> anyhow::Result<crate::acme::IssuedCert> {
            anyhow::bail!("nope")
        }
    }

    #[tokio::test]
    async fn a_failing_domain_is_not_retried_until_its_backoff_elapses() {
        let dir = std::env::temp_dir().join(format!("hoster-renewal-test-{}", std::process::id()));
        let store = CertStore::new(dir);
        let shared = SharedCerts::new(CertResolver::from_certs(&[]).unwrap());
        let wanted = vec!["*.dev.example.com".to_string()];

        let state = run_once(&AlwaysFails, &store, &shared, &wanted, Default::default(), 1000).await;
        assert_eq!(state["*.dev.example.com"].failures, 1);

        // One minute later: still inside the 15-minute backoff, so no attempt.
        let state = run_once(&AlwaysFails, &store, &shared, &wanted, state, 1060).await;
        assert_eq!(state["*.dev.example.com"].failures, 1, "must not retry during backoff");

        // After the backoff: one more attempt, one more failure.
        let state = run_once(&AlwaysFails, &store, &shared, &wanted, state, 1000 + 15 * 60 + 1).await;
        assert_eq!(state["*.dev.example.com"].failures, 2);
    }
```

- [ ] **Step 8: Wire the listener into `main.rs`**

When `settings.https_listen` is `Some`, load certificates, build `SharedCerts`, spawn the TLS listener accepting with `tokio_rustls::TlsAcceptor` and serving the same hyper service the plain listener uses, and spawn `renewal::run_loop`. The `wanted` closure recomputes the domain list from the store on each pass — the distinct project templates plus the global default reduced via `wildcard_base`, plus the control hostname — so a new project's domain is picked up without a restart.

When `https_listen` is `None`, skip all of it: no listener, no renewal loop, no issuance.

- [ ] **Step 9: Run the full suite, verify clean, commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add src/renewal.rs src/secrets.rs src/settings.rs src/main.rs src/acme.rs src/lib.rs
git commit -m "feat: ACME configuration storage and the renewal loop"
```

---

### Task 7: API and dashboard

**Files:**
- Modify: `src/api.rs`, `src/dashboard.rs`

**Interfaces:**
- Consumes: `Store::set_acme_config`, `set_dns_token`, `delete_dns_token`, `masked_acme` (Task 6); `CertStore::load_all` (Task 3).
- Produces: `PUT /acme/config`, `PUT /acme/dns`, `DELETE /acme/dns`, `GET /acme/status`; dashboard routes `POST /ui/acme/config`, `POST /ui/acme/dns`, `POST /ui/acme/dns/delete`.

- [ ] **Step 1: Write the failing tests**

Add to `src/api.rs`'s tests, reusing the existing `api_harness` / `call` / `call_without_token` / `body_string` helpers:

```rust
    #[tokio::test]
    async fn put_acme_config_then_dns_token_stores_both() {
        let (engine, settings, sessions) = api_harness();
        let res = call(&engine, &settings, &sessions, Method::PUT, "/acme/config",
            r#"{"email":"me@example.com","control_hostname":"hoster.example.com"}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let res = call(&engine, &settings, &sessions, Method::PUT, "/acme/dns",
            r#"{"kind":"cloudflare","token":"cf_secret"}"#).await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(engine.store().acme_config().unwrap().provider.unwrap().token, "cf_secret");
    }

    #[tokio::test]
    async fn acme_status_never_returns_the_dns_token() {
        let (engine, settings, sessions) = api_harness();
        engine.store().set_acme_config("me@example.com", None).unwrap();
        engine.store().set_dns_token("cloudflare", "cf_topsecret").unwrap();
        let res = call(&engine, &settings, &sessions, Method::GET, "/acme/status", "").await;
        let body = body_string(res).await;
        assert!(!body.contains("cf_topsecret"), "token leaked: {body}");
        assert!(body.contains("me@example.com"));
    }

    #[tokio::test]
    async fn put_dns_token_before_config_is_rejected() {
        let (engine, settings, sessions) = api_harness();
        let res = call(&engine, &settings, &sessions, Method::PUT, "/acme/dns",
            r#"{"kind":"cloudflare","token":"tok"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn acme_endpoints_require_the_bearer_token() {
        let (engine, settings, sessions) = api_harness();
        let res = call_without_token(&engine, &settings, &sessions, Method::PUT, "/acme/config",
            r#"{"email":"me@example.com"}"#).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
```

Add to `src/dashboard.rs`'s tests:

```rust
    #[test]
    fn tls_section_shows_setup_prompt_when_unconfigured() {
        let html = dashboard_page(&[], &[], "{service}-{branch}.dev.example.com", None, &[]);
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
        let html = dashboard_page(&[], &[], "{service}-{branch}.dev.example.com", Some(&masked), &[]);
        assert!(html.contains("me@example.com"));
        assert!(html.contains("cloudflare"));
        assert!(html.contains("••••"));
    }

    #[test]
    fn certificate_table_shows_state_per_domain() {
        let rows = vec![
            CertRow { domain: "*.dev.example.com".into(), state: "valid until 2026-10-01".into() },
            CertRow { domain: "*.demo.example.com".into(), state: "failed: no zone found".into() },
        ];
        let html = dashboard_page(&[], &[], "{service}-{branch}.dev.example.com", None, &rows);
        assert!(html.contains("*.dev.example.com"));
        assert!(html.contains("valid until 2026-10-01"));
        assert!(html.contains("no zone found"));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test --lib api dashboard`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

Add `pub struct CertRow { pub domain: String, pub state: String }` to `src/dashboard.rs`, extend `dashboard_page` to take `Option<&MaskedAcme>` and `&[CertRow]`, and render a **TLS & DNS** section above the project panels: email and control hostname with a form, the provider and a masked token with set/replace and remove controls, and the certificate table. Follow the existing panels' markup vocabulary — read `render_registry` first and match it.

Add the four API routes next to the existing `/projects` routes, inside the bearer-token gate, and the three `/ui/acme/...` form routes in `ui_projects`'s sibling handler. Order `/dns/delete` before `/dns`.

- [ ] **Step 4: Run the full suite, verify clean, commit**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

```bash
git add src/api.rs src/dashboard.rs
git commit -m "feat: TLS and DNS configuration in the API and dashboard"
```

---

### Task 8: Documentation and the systemd capability

**Files:**
- Modify: `README.md`, `scripts/install.sh`

- [ ] **Step 1: Verify every claim against the code**

Confirm route paths, body field names, env var names, and default values in `src/api.rs`, `src/main.rs`, and `src/secrets.rs`. Correct the prose wherever it drifted.

- [ ] **Step 2: Add the capability to the unit file**

In `scripts/install.sh`, in the generated systemd unit's `[Service]` section:

```
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

Without it, binding `:443` as a non-root user fails with a permission error that reads like a bug.

- [ ] **Step 3: Document it**

Add a **Built-in TLS** section to `README.md`, linked from Contents, covering: creating a Cloudflare API token scoped to `Zone:DNS:Edit` on the relevant zones; entering the ACME email, control hostname, and token in the dashboard; setting `HOSTER_HTTPS_LISTEN`; watching certificates appear in the dashboard's certificate table; and the cutover — run on `:8443` beside nginx, verify, then move to `:443` and stop nginx.

State plainly: certificates are per domain; a domain without one keeps serving plain HTTP rather than going dark; the token is stored under `0600` and never displayed again; and only Cloudflare is supported today.

- [ ] **Step 4: Commit**

```bash
git add README.md scripts/install.sh
git commit -m "docs: document built-in TLS and the nginx cutover"
```

---

## Final verification

- [ ] `cargo test` — all tests pass
- [ ] `cargo clippy --all-targets -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] `grep -rn "token" src/dashboard.rs` — no path renders a token value
- [ ] **Live issuance against Let's Encrypt staging.** No test exercises the real ACME or Cloudflare APIs, so this is the only proof the feature works. Point `instant-acme` at the staging directory (`https://acme-staging-v02.api.letsencrypt.org/directory`), configure a real Cloudflare token, and confirm a wildcard certificate is issued end to end. Staging has far looser rate limits — do **not** debug against production, where five failed authorizations per hour will lock you out.
- [ ] Verify the certificate covers a branch hostname: `openssl s_client -connect <host>:443 -servername backend-main.dev.example.com </dev/null | openssl x509 -noout -text | grep -A1 "Subject Alternative Name"`
- [ ] Confirm a domain with no certificate still serves over plain HTTP.
- [ ] Restart hoster and confirm it does **not** reissue certificates that are already valid on disk.
