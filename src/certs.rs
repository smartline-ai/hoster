//! Certificates on disk.
//!
//! One directory per domain under the configured root, holding a single
//! `0600` `cert.pem` (the chain, then the private key, written atomically as
//! one file) and a `domain` file recording the exact domain string. The
//! directory name is a collision-free encoding of the domain and is never
//! parsed back into a domain — `load_all` always reads the domain from the
//! `domain` file. Certificates outlive restarts, so hoster never reissues on
//! boot when valid ones are already present.

use std::path::PathBuf;

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

    /// The store's root directory — the natural home for files that travel
    /// alongside the certificates, such as the renewal loop's persisted
    /// per-domain backoff state.
    pub fn root(&self) -> &std::path::Path {
        &self.dir
    }

    /// The directory holding one domain's files. This mapping only needs to
    /// be collision-free, not reversible: the exact domain is written into a
    /// `domain` file inside the directory at `save()` time and read back by
    /// `load_all()`, so nothing ever parses a domain back out of this name.
    /// Existing underscores are doubled before `*` is expanded to
    /// `_wildcard_`, so e.g. `*.dev.example.com` and
    /// `_wildcard.dev.example.com` can never collide on the same directory.
    ///
    /// This does not validate `domain`; callers that write to the returned
    /// path must reject unsafe domains first (see `save`).
    pub fn dir_for(&self, domain: &str) -> PathBuf {
        let escaped = domain.replace('_', "__").replace('*', "_wildcard_");
        self.dir.join(escaped)
    }

    /// Every certificate currently on disk. Unreadable or unparseable
    /// entries — including a directory missing its `domain` file, or a
    /// certificate not yet valid at `now` — are logged and skipped, so one
    /// bad entry cannot stop startup or be handed to the SNI resolver.
    pub fn load_all(&self, now: i64) -> Vec<StoredCert> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            // Only directories are certificates. Files that legitimately live
            // in the store root — the renewal loop's `renewal-state.json`, and
            // the `.tmp` file that exists mid-`write_atomic` — are not
            // certificates that failed to load, so they must not be warned
            // about; `load_all` runs on every dashboard request, and a warning
            // per request for a file that is exactly where it belongs is noise
            // that hides the real ones.
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(domain) = std::fs::read_to_string(dir.join("domain")) else {
                tracing::warn!(dir = %dir.display(), "missing or unreadable domain file; ignoring");
                continue;
            };
            let domain = domain.trim().to_string();
            if domain.is_empty() {
                tracing::warn!(dir = %dir.display(), "empty domain file; ignoring");
                continue;
            }
            let Ok(combined_pem) = std::fs::read_to_string(dir.join("cert.pem")) else {
                tracing::warn!(dir = %dir.display(), %domain, "missing or unreadable certificate; ignoring");
                continue;
            };
            let Some((chain_pem, key_pem)) = split_chain_and_key(&combined_pem) else {
                tracing::warn!(dir = %dir.display(), %domain, "unparseable certificate; ignoring");
                continue;
            };
            let Some((not_before, not_after)) = parse_validity(&chain_pem) else {
                tracing::warn!(dir = %dir.display(), %domain, "unparseable certificate; ignoring");
                continue;
            };
            if not_before > now {
                tracing::warn!(dir = %dir.display(), %domain, "certificate not yet valid; ignoring");
                continue;
            }
            out.push(StoredCert {
                domain,
                chain_pem,
                key_pem,
                not_after,
            });
        }
        out
    }

    /// Write a certificate atomically, with the combined chain+key file
    /// owner-only from the moment it is created.
    ///
    /// `domain` must be safe to use as a single path component — no `/`,
    /// `\`, or `..`, and not itself absolute — otherwise this returns an
    /// error rather than writing outside the store root (`PathBuf::join`
    /// silently discards the base for an absolute "suffix", so an
    /// unvalidated domain could otherwise write anywhere on disk).
    pub fn save(&self, domain: &str, chain_pem: &str, key_pem: &str) -> anyhow::Result<()> {
        validate_domain(domain)?;
        let dir = self.dir_for(domain);
        std::fs::create_dir_all(&dir)?;
        // Write the domain marker first: if we crash before the cert file
        // lands, `load_all` simply treats the (cert-less) directory as
        // missing/unreadable rather than ever mismatching a domain to the
        // wrong certificate.
        write_atomic(&dir.join("domain"), domain.as_bytes(), 0o644)?;
        let mut combined = String::with_capacity(chain_pem.len() + key_pem.len() + 1);
        combined.push_str(chain_pem);
        if !chain_pem.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(key_pem);
        write_atomic(&dir.join("cert.pem"), combined.as_bytes(), 0o600)?;
        Ok(())
    }

    /// Which of `wanted` need issuing: absent, unparseable, not yet valid,
    /// or within the renewal window.
    pub fn due(&self, wanted: &[String], now: i64) -> Vec<String> {
        let have = self.load_all(now);
        wanted
            .iter()
            .filter(|d| match have.iter().find(|c| &&c.domain == d) {
                None => true,
                Some(c) => c.not_after - now <= RENEW_WITHIN_SECS,
            })
            .cloned()
            .collect()
    }
}

/// Reject domains that are not safe to use as a single filesystem path
/// component under the store root.
fn validate_domain(domain: &str) -> anyhow::Result<()> {
    if domain.is_empty() {
        anyhow::bail!("domain must not be empty");
    }
    if domain.contains('/') || domain.contains('\\') || domain.contains("..") {
        anyhow::bail!("domain {domain:?} is not safe to store on disk");
    }
    if std::path::Path::new(domain).is_absolute() {
        anyhow::bail!("domain {domain:?} is not safe to store on disk");
    }

    // Structural check: reject domains that resolve to the store root itself.
    // This guards against edge cases like "." or ".." that pass the above checks
    // but would map to the root or its parent.
    let escaped = domain.replace('_', "__").replace('*', "_wildcard_");
    if escaped == "." || escaped == ".." {
        anyhow::bail!("domain {domain:?} is not safe to store on disk");
    }

    Ok(())
}

/// Split a `save()`-written buffer (chain, then private key) back into its
/// two parts. `rustls_pemfile` can also parse certificates and a private key
/// out of one buffer directly, so this split exists only to keep
/// `StoredCert`'s two-field shape; it is not the only valid way to consume
/// the file.
fn split_chain_and_key(pem: &str) -> Option<(String, String)> {
    const KEY_MARKERS: [&str; 4] = [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ];
    let idx = KEY_MARKERS.iter().filter_map(|m| pem.find(m)).min()?;
    if idx == 0 {
        return None;
    }
    let (chain, key) = pem.split_at(idx);
    Some((chain.to_string(), key.to_string()))
}

/// `(not_before, not_after)` of the first certificate in a PEM chain, as
/// Unix timestamps, or `None` if unparseable.
fn parse_validity(pem: &str) -> Option<(i64, i64)> {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).ok()?;
    let cert = p.parse_x509().ok()?;
    let validity = cert.validity();
    Some((
        validity.not_before.timestamp(),
        validity.not_after.timestamp(),
    ))
}

/// Write `bytes` to `path` atomically: the temp file is created with `mode`
/// already in effect (never created with a default, wider mode and chmod'd
/// afterward — that would leave a window where e.g. a private key is
/// world-readable), then renamed into place.
///
/// `pub(crate)` so [`crate::renewal`] can persist its backoff state next to
/// the certificates with the same atomicity guarantee, without duplicating
/// this logic.
pub(crate) fn write_atomic(path: &std::path::Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write as _;

    let tmp = path.with_extension("tmp");
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?
    };
    #[cfg(not(unix))]
    let mut file = {
        let _ = mode;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("hoster-certs-test-{}-{n}", std::process::id()))
    }

    fn now_ts() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    /// A self-signed certificate as PEM, valid for `valid_secs` starting
    /// `not_before_offset_secs` from now (negative means already valid).
    /// Generated in-process so the test suite never needs a fixture file or
    /// a network call. Returns `(chain_pem, key_pem, not_before, not_after)`
    /// as Unix timestamps.
    fn self_signed_with_validity(
        domain: &str,
        not_before_offset_secs: i64,
        valid_secs: i64,
    ) -> (String, String, i64, i64) {
        let mut params = rcgen::CertificateParams::new(vec![domain.to_string()]).unwrap();
        let base = std::time::SystemTime::now();
        let not_before_time = if not_before_offset_secs >= 0 {
            base + std::time::Duration::from_secs(not_before_offset_secs as u64)
        } else {
            base - std::time::Duration::from_secs((-not_before_offset_secs) as u64)
        };
        let not_after_time =
            not_before_time + std::time::Duration::from_secs(valid_secs.max(1) as u64);
        params.not_before = not_before_time.into();
        params.not_after = not_after_time.into();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let not_before_ts = not_before_time
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let not_after_ts = not_before_ts + valid_secs;
        (cert.pem(), key.serialize_pem(), not_before_ts, not_after_ts)
    }

    /// A self-signed certificate valid for one hour, as PEM, starting now.
    fn self_signed(domain: &str, valid_secs: i64) -> (String, String, i64) {
        let (chain, key, _not_before, not_after) = self_signed_with_validity(domain, 0, valid_secs);
        (chain, key, not_after)
    }

    #[test]
    fn save_then_load_round_trips() {
        let store = CertStore::new(temp_dir());
        let (chain, key, _) = self_signed("dev.example.com", 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        let all = store.load_all(now_ts());
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
            let mode = std::fs::metadata(dir.join("cert.pem"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
        }
    }

    /// We cannot race-free observe, from outside the process, that no wider
    /// permission window ever existed for the temp file without an
    /// intrusive fault-injection harness (e.g. pausing the writer between
    /// open() and rename()). As a documented limitation, this test instead
    /// pins the *technique*: `write_atomic` must set the final mode via
    /// `OpenOptions::mode` at creation time, not via `fs::write` followed by
    /// a later `set_permissions` (which is exactly the bug being fixed —
    /// see git history for the before/after).
    #[test]
    fn write_atomic_sets_mode_at_creation_not_after() {
        let src = include_str!("certs.rs");
        assert!(
            src.contains(".mode(mode)"),
            "write_atomic must pass the final mode to OpenOptions at creation time"
        );
        let write_atomic_src = src
            .split("fn write_atomic(")
            .nth(1)
            .expect("write_atomic function present");
        assert!(
            !write_atomic_src.contains("std::fs::write(&tmp"),
            "write_atomic must not create the file with fs::write (default mode) and chmod later"
        );
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
        assert!(
            store
                .due(&["*.dev.example.com".to_string()], now)
                .is_empty()
        );
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

    /// Pins the behaviour exactly at the renewal threshold: `due()` uses
    /// `<=`, so a certificate with `not_after - now == RENEW_WITHIN_SECS` is
    /// due (erring toward renewing too early over too late).
    #[test]
    fn due_includes_a_certificate_exactly_at_the_renewal_threshold() {
        let store = CertStore::new(temp_dir());
        let (chain, key, not_after) = self_signed("*.dev.example.com", 90 * 24 * 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        let now = not_after - RENEW_WITHIN_SECS;
        assert_eq!(
            store.due(&["*.dev.example.com".to_string()], now),
            vec!["*.dev.example.com".to_string()],
            "a certificate exactly at the renewal threshold must be due"
        );
    }

    #[test]
    fn an_unparseable_certificate_is_treated_as_absent_not_a_panic() {
        let store = CertStore::new(temp_dir());
        let dir = store.dir_for("*.dev.example.com");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("domain"), b"*.dev.example.com").unwrap();
        std::fs::write(dir.join("cert.pem"), b"not a certificate").unwrap();
        let now = now_ts();
        assert!(
            store.load_all(now).is_empty(),
            "corrupt files must not load"
        );
        assert_eq!(
            store.due(&["*.dev.example.com".to_string()], now),
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

    /// The original bug: `dir_for` mapped both `*.dev.example.com` and the
    /// (unrelated) domain `_wildcard.dev.example.com` to the same directory,
    /// so saving one silently clobbered the other, and loading corrupted
    /// any domain merely containing the substring `_wildcard`. Both domains
    /// must now round-trip independently.
    #[test]
    fn a_wildcard_domain_and_a_literal_wildcard_text_domain_do_not_collide() {
        let store = CertStore::new(temp_dir());
        let (chain_a, key_a, _) = self_signed("*.dev.example.com", 3600);
        let (chain_b, key_b, _) = self_signed("_wildcard.dev.example.com", 3600);

        assert_ne!(
            store.dir_for("*.dev.example.com"),
            store.dir_for("_wildcard.dev.example.com"),
            "distinct domains must map to distinct directories"
        );

        store.save("*.dev.example.com", &chain_a, &key_a).unwrap();
        store
            .save("_wildcard.dev.example.com", &chain_b, &key_b)
            .unwrap();

        let all = store.load_all(now_ts());
        let mut domains: Vec<String> = all.iter().map(|c| c.domain.clone()).collect();
        domains.sort();
        assert_eq!(
            domains,
            vec![
                "*.dev.example.com".to_string(),
                "_wildcard.dev.example.com".to_string(),
            ],
            "each domain must load back with its own exact name; neither overwrites the other"
        );
    }

    #[test]
    fn save_rejects_a_domain_that_could_escape_the_store_root() {
        let root = temp_dir();
        let store = CertStore::new(root.clone());
        let (chain, key, _) = self_signed("dev.example.com", 3600);

        for bad in [
            "../../etc/evil",
            "foo/../../bar",
            "a/b",
            "a\\b",
            "/etc/evil",
            "..",
        ] {
            let result = store.save(bad, &chain, &key);
            assert!(result.is_err(), "expected {bad:?} to be rejected");
        }

        assert!(
            !root.exists(),
            "nothing should have been written under the store root"
        );
    }

    #[test]
    fn a_not_yet_valid_certificate_is_reported_as_due_and_hidden_from_load_all() {
        let store = CertStore::new(temp_dir());
        // Starts 1 hour from now.
        let (chain, key, not_before, _not_after) =
            self_signed_with_validity("*.dev.example.com", 3600, 90 * 24 * 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();

        let now = not_before - 1800; // 30 minutes before it becomes valid.
        assert!(
            store.load_all(now).is_empty(),
            "a not-yet-valid certificate must not be offered to the resolver"
        );
        assert_eq!(
            store.due(&["*.dev.example.com".to_string()], now),
            vec!["*.dev.example.com".to_string()],
            "a not-yet-valid certificate must be reported as due"
        );
    }

    #[test]
    fn load_all_skips_a_directory_missing_its_domain_file_without_panicking() {
        let store = CertStore::new(temp_dir());
        let (chain, key, _) = self_signed("dev.example.com", 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        // Simulate a crash before the domain marker was written, or a
        // directory some other process left behind.
        std::fs::remove_file(store.dir_for("*.dev.example.com").join("domain")).unwrap();
        assert!(store.load_all(now_ts()).is_empty());
    }

    /// The renewal loop persists `renewal-state.json` in the store root, and
    /// `write_atomic` leaves a `.tmp` sibling mid-write. Neither is a
    /// certificate directory, so the scan must pass over both silently rather
    /// than treating them as certificates that failed to load.
    #[test]
    fn load_all_ignores_plain_files_in_the_store_root() {
        let root = temp_dir();
        let store = CertStore::new(root.clone());
        let (chain, key, _) = self_signed("dev.example.com", 3600);
        store.save("*.dev.example.com", &chain, &key).unwrap();
        std::fs::write(root.join("renewal-state.json"), b"{}").unwrap();
        std::fs::write(root.join("renewal-state.json.tmp"), b"{").unwrap();

        let all = store.load_all(now_ts());
        assert_eq!(all.len(), 1, "only the certificate directory is a cert");
        assert_eq!(all[0].domain, "*.dev.example.com");
    }

    #[test]
    fn save_rejects_a_single_dot_domain() {
        let root = temp_dir();
        let store = CertStore::new(root.clone());
        let (chain, key, _) = self_signed("dev.example.com", 3600);

        let result = store.save(".", &chain, &key);
        assert!(result.is_err(), "domain '.' should be rejected");

        // Verify nothing was written to the store root.
        assert!(
            !root.join("cert.pem").exists(),
            "cert.pem should not exist in store root"
        );
        assert!(
            !root.join("domain").exists(),
            "domain file should not exist in store root"
        );
    }

    #[test]
    fn save_rejects_a_double_dot_domain() {
        let root = temp_dir();
        let store = CertStore::new(root.clone());
        let (chain, key, _) = self_signed("dev.example.com", 3600);

        let result = store.save("..", &chain, &key);
        assert!(result.is_err(), "domain '..' should be rejected");

        // Verify nothing was written to the store root.
        assert!(
            !root.join("cert.pem").exists(),
            "cert.pem should not exist in store root"
        );
        assert!(
            !root.join("domain").exists(),
            "domain file should not exist in store root"
        );
    }

    #[test]
    fn save_rejects_a_slash_dot_slash_dot_domain() {
        let root = temp_dir();
        let store = CertStore::new(root.clone());
        let (chain, key, _) = self_signed("dev.example.com", 3600);

        let result = store.save("./.", &chain, &key);
        assert!(result.is_err(), "domain './.' should be rejected");

        // Verify nothing was written to the store root.
        assert!(
            !root.join("cert.pem").exists(),
            "cert.pem should not exist in store root"
        );
        assert!(
            !root.join("domain").exists(),
            "domain file should not exist in store root"
        );
    }
}

/// One row of the certificate table: a domain hoster wants a certificate for,
/// plus a free-form, human-readable summary of its current state — `"valid
/// until 2026-10-01"`, `"failed: no zone found"`, `"pending"`.
///
/// Built by the caller from a [`CertStore`] and the renewal loop's persisted
/// state. Served by `GET /acme/status` and rendered by the dashboard's TLS
/// panel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CertRow {
    pub domain: String,
    pub state: String,
}
