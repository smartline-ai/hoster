//! TLS termination with per-domain certificates selected by SNI.

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
    /// Build from stored certificates. Entries that fail to parse, or whose
    /// key does not match its certificate, are skipped with a warning rather
    /// than failing the whole resolver — one bad certificate must not remove
    /// TLS for every other domain.
    pub fn from_certs(certs: &[StoredCert]) -> anyhow::Result<Self> {
        let provider = rustls::crypto::ring::default_provider();
        let mut entries = Vec::new();
        for c in certs {
            match build_certified_key(&provider, &c.chain_pem, &c.key_pem) {
                Ok(ck) => entries.push((c.domain.to_ascii_lowercase(), Arc::new(ck))),
                Err(e) => {
                    tracing::warn!(domain = %c.domain, error = %e, "skipping unusable certificate")
                }
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

/// Parse a PEM chain and key into a `CertifiedKey`, verifying (where
/// possible) that the key actually matches the certificate — a mismatched
/// pair is caught here rather than surfacing as a handshake failure later.
fn build_certified_key(
    provider: &rustls::crypto::CryptoProvider,
    chain_pem: &str,
    key_pem: &str,
) -> anyhow::Result<CertifiedKey> {
    let chain: Vec<_> =
        rustls_pemfile::certs(&mut chain_pem.as_bytes()).collect::<Result<_, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or_else(|| anyhow::anyhow!("no private key in PEM"))?;
    CertifiedKey::from_der(chain, key, provider).map_err(anyhow::Error::from)
}

/// The live TLS config, swappable without dropping connections — the same
/// `Arc<ArcSwap<_>>` pattern `SharedRoutes` uses for the routing table.
#[derive(Clone)]
pub struct SharedCerts(Arc<ArcSwap<ServerConfig>>);

impl SharedCerts {
    pub fn new(resolver: CertResolver) -> Self {
        SharedCerts(Arc::new(ArcSwap::from_pointee(config_from(resolver))))
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
                prefix.ends_with('.')
                    && !prefix.is_empty()
                    && !prefix[..prefix.len() - 1].contains('.')
            }
            None => false,
        },
    }
}

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
        assert!(!matches(
            "*.dev.example.com",
            "backend.dev.example.com.evil.com"
        ));
    }

    /// The given "different suffix" test above is rejected because the SNI
    /// doesn't end with the parent domain at all. This pins the sharper,
    /// distinct case: an SNI that genuinely ends with the *characters* of
    /// the parent domain but with no label boundary in front of them, so it
    /// is not actually a subdomain of the parent at all. `notdev.example.com`
    /// is the single label `notdev` under `example.com` — a different name
    /// entirely from `dev.example.com` — even though the string ends with
    /// `dev.example.com`. A naive `ends_with(parent)` check (without also
    /// requiring the preceding character to be `.`) would wrongly accept it.
    #[test]
    fn wildcard_does_not_match_without_a_label_boundary() {
        assert!(!matches("*.dev.example.com", "notdev.example.com"));
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

    #[test]
    fn resolver_skips_an_unusable_certificate_without_failing() {
        let bad = StoredCert {
            domain: "*.dev.example.com".into(),
            chain_pem: "not a cert".into(),
            key_pem: "not a key".into(),
            not_after: i64::MAX,
        };
        let r = CertResolver::from_certs(&[bad]).unwrap();
        assert!(
            r.entries.is_empty(),
            "unusable certificate should be skipped, not fatal"
        );
    }

    /// A genuine, in-process self-signed certificate — no fixture file, no
    /// network call.
    fn self_signed(domain: &str) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec![domain.to_string()]).unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn from_certs_builds_a_usable_entry_for_a_valid_certificate() {
        let (chain_pem, key_pem) = self_signed("dev.example.com");
        let stored = StoredCert {
            domain: "*.dev.example.com".into(),
            chain_pem,
            key_pem,
            not_after: i64::MAX,
        };
        let r = CertResolver::from_certs(&[stored]).unwrap();
        assert_eq!(
            r.entries.len(),
            1,
            "a valid certificate must produce a usable entry"
        );
        assert_eq!(r.entries[0].0, "*.dev.example.com");
    }

    #[test]
    fn shared_certs_swap_publishes_a_new_config_without_mutating_the_old_one() {
        let empty = CertResolver::from_certs(&[]).unwrap();
        let shared = SharedCerts::new(empty);
        let before = shared.server_config();

        let (chain_pem, key_pem) = self_signed("dev.example.com");
        let stored = StoredCert {
            domain: "dev.example.com".into(),
            chain_pem,
            key_pem,
            not_after: i64::MAX,
        };
        let resolver = CertResolver::from_certs(&[stored]).unwrap();
        shared.swap(resolver);
        let after = shared.server_config();

        assert!(
            !Arc::ptr_eq(&before, &after),
            "swap must publish a new config rather than mutate the one \
             in-flight connections already hold"
        );
    }
}
