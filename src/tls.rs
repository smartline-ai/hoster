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
///
/// A single trailing dot (an absolute FQDN, e.g. `backend.dev.example.com.`)
/// is normalised away on both sides before comparing: rustls accepts a
/// trailing dot in a `ClientHello`'s server name and passes it through
/// untrimmed, so a client sending an absolute name must still be matched
/// against a certificate issued for the relative form.
pub fn matches(cert_domain: &str, sni: &str) -> bool {
    let cert = normalize(cert_domain);
    let sni = normalize(sni);
    match cert.strip_prefix("*.") {
        None => cert == sni,
        Some(parent) => match sni.strip_suffix(parent) {
            // The remaining prefix must be exactly one non-empty label plus its dot.
            Some(prefix) => match prefix.strip_suffix('.') {
                Some(label) => !label.is_empty() && !label.contains('.'),
                None => false,
            },
            None => false,
        },
    }
}

/// Lowercase a name and strip a single trailing dot, if present.
fn normalize(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.strip_suffix('.') {
        Some(trimmed) => trimmed.to_string(),
        None => lower,
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
    fn trailing_dot_on_sni_is_normalised() {
        assert!(matches("*.dev.example.com", "backend.dev.example.com."));
        assert!(matches("hoster.example.com", "hoster.example.com."));
    }

    #[test]
    fn trailing_dot_on_cert_domain_is_normalised() {
        assert!(matches("*.dev.example.com.", "backend.dev.example.com"));
        assert!(matches("hoster.example.com.", "hoster.example.com"));
    }

    #[test]
    fn trailing_dot_on_both_sides_is_normalised() {
        assert!(matches("*.dev.example.com.", "backend.dev.example.com."));
    }

    #[test]
    fn wildcard_rejects_an_empty_label() {
        assert!(!matches("*.dev.example.com", ".dev.example.com"));
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
    fn resolver_skips_a_certificate_whose_key_does_not_match() {
        // Two independently generated, individually valid certificate/key
        // pairs. Pairing one certificate with the *other* pair's private
        // key must be caught by `CertifiedKey::from_der`'s key/cert match
        // check, not silently accepted and only fail at handshake time.
        let (chain_pem, _matching_key_pem) = self_signed("dev.example.com");
        let (_other_chain_pem, mismatched_key_pem) = self_signed("other.example.com");
        let stored = StoredCert {
            domain: "dev.example.com".into(),
            chain_pem,
            key_pem: mismatched_key_pem,
            not_after: i64::MAX,
        };
        let r = CertResolver::from_certs(&[stored]).unwrap();
        assert!(
            r.entries.is_empty(),
            "a certificate paired with a non-matching private key must be \
             skipped, not served"
        );
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

    // `ClientHello` has no public constructor outside rustls, so `resolve()`
    // cannot be reached with a hand-built value — a mutation to
    // `self.entries.first()` would still typecheck and every test above
    // would keep passing. These tests drive a real handshake instead: a
    // `TlsAcceptor` built from `SharedCerts::server_config()` against a
    // `TlsConnector`, over an in-memory `tokio::io::duplex` pair (no
    // sockets, no network), with the client trusting the exact self-signed
    // roots involved. That's the only way to observe which certificate
    // `resolve()` actually chose.
    mod handshake {
        use std::sync::Arc;

        use rustls::RootCertStore;
        use rustls::pki_types::{CertificateDer, ServerName};
        use tokio_rustls::{TlsAcceptor, TlsConnector};

        use super::*;

        /// A self-signed certificate for `domain`, generated in-process via
        /// `rcgen`. The certificate's own SAN is `domain`, so a `StoredCert`
        /// built from it and looked up under the same `domain` behaves like
        /// a real cert/entry pair — including under rustls's own wildcard
        /// hostname verification on the client side.
        fn stored_cert(domain: &str) -> (StoredCert, CertificateDer<'static>) {
            let key = rcgen::KeyPair::generate().unwrap();
            let params = rcgen::CertificateParams::new(vec![domain.to_string()]).unwrap();
            let cert = params.self_signed(&key).unwrap();
            let der = cert.der().clone();
            let stored = StoredCert {
                domain: domain.into(),
                chain_pem: cert.pem(),
                key_pem: key.serialize_pem(),
                not_after: i64::MAX,
            };
            (stored, der)
        }

        /// Drive a real TLS handshake over an in-memory duplex pair, server
        /// side backed by `server_certs` through the real `CertResolver` /
        /// `SharedCerts` path, client side trusting exactly `trusted` as
        /// roots. Returns the DER of the certificate the client actually
        /// received (proof of what `resolve()` picked), or the handshake
        /// error if either side failed.
        async fn handshake(
            server_certs: &[StoredCert],
            trusted: &[CertificateDer<'static>],
            sni: &str,
        ) -> anyhow::Result<CertificateDer<'static>> {
            let resolver = CertResolver::from_certs(server_certs)?;
            let shared = SharedCerts::new(resolver);
            let acceptor = TlsAcceptor::from(shared.server_config());

            let mut roots = RootCertStore::empty();
            for der in trusted {
                roots.add(der.clone())?;
            }
            let client_config = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = TlsConnector::from(Arc::new(client_config));

            let (client_io, server_io) = tokio::io::duplex(16 * 1024);
            let server_name = ServerName::try_from(sni.to_string())?;

            let server = tokio::spawn(async move { acceptor.accept(server_io).await });
            let client =
                tokio::spawn(async move { connector.connect(server_name, client_io).await });

            let server_result = server.await.expect("server task panicked");
            let client_result = client.await.expect("client task panicked");

            server_result.map_err(|e| anyhow::anyhow!("server-side handshake failed: {e}"))?;
            let client_stream =
                client_result.map_err(|e| anyhow::anyhow!("client-side handshake failed: {e}"))?;

            let (_, conn) = client_stream.get_ref();
            conn.peer_certificates()
                .and_then(|certs| certs.first())
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("client completed handshake with no peer certificate")
                })
        }

        /// With certificates for `*.dev.example.com` and `other.example.com`
        /// loaded, a client using SNI `backend-main.dev.example.com` must
        /// receive the `*.dev.example.com` certificate.
        ///
        /// A resolver mutated to `self.entries.first()` must fail this: the
        /// unrelated certificate is deliberately placed first in the vec, so
        /// "first" and "correct" are never the same entry here.
        #[tokio::test]
        async fn resolve_selects_the_wildcard_certificate_for_a_covered_subdomain() {
            let (wildcard_cert, wildcard_der) = stored_cert("*.dev.example.com");
            let (other_cert, _other_der) = stored_cert("other.example.com");
            let (decoy_cert, _decoy_der) = stored_cert("unrelated.example.net");

            // Deliberately not first: `entries.first()` would be `decoy_cert`.
            let certs = vec![decoy_cert, other_cert, wildcard_cert];
            let trusted = vec![wildcard_der.clone()];

            let received = handshake(&certs, &trusted, "backend-main.dev.example.com")
                .await
                .expect("handshake with a covered SNI must succeed");

            assert_eq!(
                received, wildcard_der,
                "client must receive the *.dev.example.com certificate for \
                 a subdomain it covers"
            );
        }

        /// The exact-match sibling of the above: SNI `other.example.com`
        /// must receive the `other.example.com` certificate, not the
        /// wildcard one — and not whatever `entries.first()` happens to be.
        #[tokio::test]
        async fn resolve_selects_the_exact_certificate_for_an_exact_sni() {
            let (wildcard_cert, _wildcard_der) = stored_cert("*.dev.example.com");
            let (other_cert, other_der) = stored_cert("other.example.com");
            let (decoy_cert, _decoy_der) = stored_cert("unrelated.example.net");

            // Deliberately not first: `entries.first()` would be `decoy_cert`.
            let certs = vec![decoy_cert, wildcard_cert, other_cert];
            let trusted = vec![other_der.clone()];

            let received = handshake(&certs, &trusted, "other.example.com")
                .await
                .expect("handshake with an exact-match SNI must succeed");

            assert_eq!(
                received, other_der,
                "client must receive the other.example.com certificate for \
                 an exact-match SNI"
            );
        }

        /// An SNI that no loaded certificate covers must fail the handshake
        /// rather than being served an arbitrary certificate — `resolve()`
        /// returns `None`, and rustls sends a fatal alert instead of a cert.
        #[tokio::test]
        async fn resolve_fails_the_handshake_for_an_uncovered_sni() {
            let (wildcard_cert, wildcard_der) = stored_cert("*.dev.example.com");
            let (other_cert, other_der) = stored_cert("other.example.com");
            let certs = vec![wildcard_cert, other_cert];
            let trusted = vec![wildcard_der, other_der];

            let result = handshake(&certs, &trusted, "totally-unrelated.example.org").await;

            assert!(
                result.is_err(),
                "a client offering an SNI no certificate covers must not \
                 complete the handshake"
            );
        }
    }
}
