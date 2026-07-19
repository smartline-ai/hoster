//! The background certificate renewal loop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::acme::CertIssuer;
use crate::certs::{CertStore, StoredCert};
use crate::tls::{CertResolver, SharedCerts};

const BASE_BACKOFF_SECS: i64 = 15 * 60;
const MAX_BACKOFF_SECS: i64 = 24 * 3600;

/// How often a renewal pass runs. Certificates are renewed 30 days before
/// expiry, so a six-hourly pass has ample margin while keeping the retry
/// cadence for a newly configured domain reasonable.
const PASS_INTERVAL: Duration = Duration::from_secs(6 * 3600);

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

/// Per-domain failure state, for backoff and for display.
#[derive(Debug, Clone, Default)]
pub struct DomainState {
    pub failures: u32,
    pub last_attempt: i64,
    pub last_error: Option<String>,
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run one renewal pass: issue every domain that is due and not in backoff.
/// Returns the updated failure state.
pub async fn run_once(
    issuer: &dyn CertIssuer,
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
        rebuild(store, shared, now);
    }

    // Domains that are no longer wanted (a project's template changed, say)
    // must not keep their failure state forever — it would resurrect stale
    // backoff if the domain is ever wanted again.
    state.retain(|domain, _| wanted.iter().any(|w| w == domain));
    state
}

/// Rebuild the live TLS config from what is on disk.
fn rebuild(store: &CertStore, shared: &SharedCerts, now: i64) {
    let certs: Vec<StoredCert> = store.load_all(now);
    match CertResolver::from_certs(&certs) {
        Ok(r) => shared.swap(r),
        Err(e) => tracing::error!(error = %e, "failed to rebuild the certificate resolver"),
    }
}

/// Every six hours, run a renewal pass.
///
/// `wanted` is re-evaluated on every pass rather than captured once, so a
/// project configured after startup gets a certificate without a restart.
pub async fn run_loop(
    issuer: Arc<dyn CertIssuer>,
    store: Arc<CertStore>,
    shared: SharedCerts,
    wanted: impl Fn() -> Vec<String> + Send + 'static,
) {
    let mut state = BTreeMap::new();
    loop {
        let now = now_secs();
        state = run_once(issuer.as_ref(), &store, &shared, &wanted(), state, now).await;
        tokio::time::sleep(PASS_INTERVAL).await;
    }
}

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

    #[test]
    fn backoff_never_overflows_at_the_extreme() {
        // u32::MAX failures must still produce a sane, capped delay rather
        // than wrapping negative and retrying instantly.
        assert_eq!(next_attempt(u32::MAX, 0), 24 * 3600);
    }

    struct AlwaysFails;

    #[async_trait::async_trait]
    impl crate::acme::CertIssuer for AlwaysFails {
        async fn issue(&self, _domain: &str) -> anyhow::Result<crate::acme::IssuedCert> {
            anyhow::bail!("nope")
        }
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "hoster-renewal-test-{}-{n}-{name}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn a_failing_domain_is_not_retried_until_its_backoff_elapses() {
        let store = CertStore::new(temp_dir("backoff"));
        let shared = SharedCerts::new(CertResolver::from_certs(&[]).unwrap());
        let wanted = vec!["*.dev.example.com".to_string()];

        let state = run_once(
            &AlwaysFails,
            &store,
            &shared,
            &wanted,
            Default::default(),
            1000,
        )
        .await;
        assert_eq!(state["*.dev.example.com"].failures, 1);

        // One minute later: still inside the 15-minute backoff, so no attempt.
        let state = run_once(&AlwaysFails, &store, &shared, &wanted, state, 1060).await;
        assert_eq!(
            state["*.dev.example.com"].failures, 1,
            "must not retry during backoff"
        );

        // After the backoff: one more attempt, one more failure.
        let state = run_once(
            &AlwaysFails,
            &store,
            &shared,
            &wanted,
            state,
            1000 + 15 * 60 + 1,
        )
        .await;
        assert_eq!(state["*.dev.example.com"].failures, 2);
        assert!(state["*.dev.example.com"].last_error.is_some());
    }

    #[tokio::test]
    async fn a_domain_no_longer_wanted_is_dropped_from_the_state() {
        let store = CertStore::new(temp_dir("forget"));
        let shared = SharedCerts::new(CertResolver::from_certs(&[]).unwrap());

        let state = run_once(
            &AlwaysFails,
            &store,
            &shared,
            &["*.old.example.com".to_string()],
            Default::default(),
            1000,
        )
        .await;
        assert!(state.contains_key("*.old.example.com"));

        let state = run_once(
            &AlwaysFails,
            &store,
            &shared,
            &["*.new.example.com".to_string()],
            state,
            2000,
        )
        .await;
        assert!(
            !state.contains_key("*.old.example.com"),
            "state for a domain that is no longer wanted must not linger"
        );
    }
}
