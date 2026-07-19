//! Certificates on disk.
//!
//! One directory per domain under the configured root, each holding
//! `fullchain.pem` and a `0600` `key.pem`. Certificates outlive restarts, so
//! hoster never reissues on boot when valid ones are already present.

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
            let name = entry
                .file_name()
                .to_string_lossy()
                .replace("_wildcard", "*");
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
            .filter(|d| match have.iter().find(|c| &&c.domain == d) {
                None => true,
                Some(c) => c.not_after - now <= RENEW_WITHIN_SECS,
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
            let mode = std::fs::metadata(dir.join("key.pem"))
                .unwrap()
                .permissions()
                .mode();
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
