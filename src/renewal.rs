//! The background certificate renewal loop.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::acme::CertIssuer;
use crate::certs::{CertStore, StoredCert};
use crate::secrets::Store;
use crate::settings::wildcard_base;
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

/// Every domain hoster wants a certificate for: the wildcard base of the
/// default hostname template, of every project's own template, and the ACME
/// control hostname if one is configured (hoster serves that name on the
/// HTTPS listener, so it needs a certificate like any other).
///
/// The renewal loop and the dashboard's certificate table must agree on this
/// set exactly — when they drift the dashboard silently misreports which
/// domains are managed — so both call this one function rather than keeping
/// their own copy of the derivation.
pub fn wanted_domains(store: &Store, default_template: &str) -> Vec<String> {
    let mut out: Vec<String> = std::iter::once(default_template.to_string())
        .chain(store.project_hostname_templates())
        .filter_map(|t| wildcard_base(&t))
        .collect();
    if let Some(h) = store.masked_acme().and_then(|a| a.control_hostname) {
        out.push(h);
    }
    out.sort();
    out.dedup();
    out
}

/// A handle an operator-facing endpoint uses to ask the renewal loop to run a
/// pass now, instead of waiting up to six hours for the next scheduled one.
///
/// This exists because the alternative is unusable: an operator who pastes a
/// Cloudflare token into the dashboard would otherwise watch
/// `failed: ACME is not configured` sit there for hours — longer, since the
/// domain has accumulated backoff from the failures the *missing*
/// configuration caused.
///
/// `notify_one` stores a permit when nobody is waiting, so a trigger fired
/// while a pass is already running is remembered and honoured by the next
/// wait rather than lost.
#[derive(Clone, Default)]
pub struct RenewalTrigger {
    notify: Arc<tokio::sync::Notify>,
}

impl RenewalTrigger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a pass. Never blocks, and is safe to call when no loop is
    /// running (the permit is simply never consumed).
    pub fn request(&self) {
        self.notify.notify_one();
    }

    /// Resolve when a pass has been requested.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

/// Forget the accumulated backoff for `domains`.
///
/// Used only on a manually triggered pass: those failures were caused by the
/// configuration the operator has just changed, so continuing to honour their
/// backoff would make the retry they asked for do nothing. Failures that
/// happen *after* the trigger accumulate backoff as usual — Let's Encrypt's
/// rate limits do not care why we retried.
pub fn clear_backoff(state: &mut BTreeMap<String, DomainState>, domains: &[String]) {
    state.retain(|domain, _| !domains.iter().any(|d| d == domain));
}

/// Per-domain failure state, for backoff and for display.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DomainState {
    pub failures: u32,
    pub last_attempt: i64,
    pub last_error: Option<String>,
}

/// Where per-domain renewal state is persisted: alongside the certificates,
/// under the store's root — the natural home, per the design this fixes.
fn state_path(store: &CertStore) -> PathBuf {
    store.root().join("renewal-state.json")
}

/// Persist per-domain failure state atomically, the same way certificates
/// are written, so a crash mid-write never leaves a half-written (and thus
/// unparseable) file behind.
///
/// This is the fix for a restart resetting the backoff: without it, a crash
/// loop (a flapping Docker socket, say) reissues with zero backoff on every
/// boot, and five restarts within an hour can exhaust Let's Encrypt's
/// five-authorization-failures-per-identifier-per-hour limit.
pub fn save_state(store: &CertStore, state: &BTreeMap<String, DomainState>) -> anyhow::Result<()> {
    let path = state_path(store);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(state)?;
    crate::certs::write_atomic(&path, json.as_bytes(), 0o600)
}

/// Load persisted per-domain failure state. A missing file (first boot, or
/// an upgrade from a version that didn't persist state) or an unparseable
/// one (a corrupt write, a foreign file) is treated as empty state rather
/// than a startup failure — this file is an optimization the loop can do
/// without, not a source of truth it depends on.
pub fn load_state(store: &CertStore) -> BTreeMap<String, DomainState> {
    let raw = match std::fs::read_to_string(state_path(store)) {
        Ok(raw) => raw,
        Err(_) => return BTreeMap::new(),
    };
    match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(e) => {
            tracing::warn!(error = %e, "renewal state file is corrupt; starting with empty state");
            BTreeMap::new()
        }
    }
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
        // Scoped so this borrow of `state` ends before the entry is
        // re-acquired below — `state.remove` on a clean issuance needs
        // `state` free again, and re-borrowing per branch (rather than
        // holding one `&mut DomainState` across the whole match) is what
        // makes that possible.
        {
            let st = state.entry(domain.clone()).or_default();
            if now < next_attempt(st.failures, st.last_attempt) {
                continue;
            }
            st.last_attempt = now;
        }
        match issuer.issue(&domain).await {
            Ok(cert) => match store.save(&domain, &cert.chain_pem, &cert.key_pem) {
                Ok(()) => {
                    tracing::info!(domain = %domain, "certificate issued");
                    // Clear the domain's entry entirely, rather than merely
                    // zeroing its counters, so a successful issuance doesn't
                    // leave stale bookkeeping (e.g. a `last_error` from a
                    // prior failure) sitting in the persisted state file.
                    state.remove(&domain);
                    changed = true;
                }
                Err(e) => {
                    tracing::error!(domain = %domain, error = %e, "failed to save certificate");
                    let st = state.entry(domain.clone()).or_default();
                    st.failures += 1;
                    st.last_error = Some(e.to_string());
                }
            },
            Err(e) => {
                tracing::warn!(domain = %domain, error = %e, "certificate issuance failed");
                let st = state.entry(domain.clone()).or_default();
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
///
/// Failure state is loaded from disk before the first pass and saved after
/// every pass, so a restart (a deploy, a crash, a flapping Docker socket)
/// does not reset backoff to zero — see [`save_state`] for why that matters.
pub async fn run_loop(
    issuer: Arc<dyn CertIssuer>,
    store: Arc<CertStore>,
    shared: SharedCerts,
    wanted: impl Fn() -> Vec<String> + Send + 'static,
    trigger: RenewalTrigger,
) {
    let mut state = load_state(&store);
    // The first pass at startup is scheduled, not operator-requested.
    let mut manual = false;
    loop {
        let domains = wanted();
        if manual {
            // The operator asked for this pass, having just changed the
            // configuration the previous failures were caused by.
            clear_backoff(&mut state, &domains);
        }
        let now = now_secs();
        state = run_once(issuer.as_ref(), &store, &shared, &domains, state, now).await;
        if let Err(e) = save_state(&store, &state) {
            tracing::error!(error = %e, "failed to persist renewal state");
        }
        manual = tokio::select! {
            _ = tokio::time::sleep(PASS_INTERVAL) => false,
            _ = trigger.wait() => {
                tracing::info!("renewal pass requested by operator");
                true
            }
        };
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

    /// A store on a unique, non-existent path, so tests never share state.
    fn temp_store() -> Store {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let n = C.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "hoster-renewal-unit-{}-{n}/projects.json",
            std::process::id()
        ));
        Store::load(path).unwrap()
    }

    #[test]
    fn wanted_domains_covers_the_default_template_projects_and_the_control_host() {
        let store = temp_store();
        store
            .set_hostname_template("proj", "{service}-{branch}.team.example.com")
            .unwrap();
        store
            .set_acme_config("me@example.com", Some("hoster.example.com"))
            .unwrap();
        assert_eq!(
            wanted_domains(&store, "{service}-{branch}.dev.example.com"),
            vec![
                "*.dev.example.com".to_string(),
                "*.team.example.com".to_string(),
                "hoster.example.com".to_string(),
            ]
        );
    }

    #[test]
    fn wanted_domains_dedupes_a_project_template_equal_to_the_default() {
        let store = temp_store();
        store
            .set_hostname_template("proj", "{service}-{branch}.dev.example.com")
            .unwrap();
        assert_eq!(
            wanted_domains(&store, "{service}-{branch}.dev.example.com"),
            vec!["*.dev.example.com".to_string()]
        );
    }

    #[test]
    fn wanted_domains_omits_the_control_host_when_unset() {
        let store = temp_store();
        store.set_acme_config("me@example.com", None).unwrap();
        assert_eq!(
            wanted_domains(&store, "{service}-{branch}.dev.example.com"),
            vec!["*.dev.example.com".to_string()]
        );
    }

    #[test]
    fn clearing_backoff_only_touches_the_domains_being_retried() {
        let mut state = BTreeMap::new();
        state.insert(
            "*.dev.example.com".to_string(),
            DomainState {
                failures: 4,
                last_attempt: 1000,
                last_error: Some("ACME is not configured".into()),
            },
        );
        state.insert(
            "other.example.com".to_string(),
            DomainState {
                failures: 2,
                last_attempt: 1000,
                last_error: Some("nope".into()),
            },
        );
        clear_backoff(&mut state, &["*.dev.example.com".to_string()]);
        assert!(!state.contains_key("*.dev.example.com"));
        assert_eq!(state["other.example.com"].failures, 2);
    }

    /// Clearing backoff is what actually makes a manual retry do something:
    /// with the failure state still in place the pass skips the domain.
    #[tokio::test]
    async fn a_domain_in_backoff_is_skipped_until_the_backoff_is_cleared() {
        let store = CertStore::new(temp_dir("manual-retry"));
        let shared = SharedCerts::new(CertResolver::from_certs(&[]).unwrap());
        let wanted = vec!["a.example.com".to_string()];

        // One failure puts the domain into a 15-minute backoff.
        let state = run_once(&AlwaysFails, &store, &shared, &wanted, BTreeMap::new(), 0).await;
        assert_eq!(state["a.example.com"].failures, 1);

        // A pass one minute later does not even attempt it: the domain is
        // still in backoff, so nothing is written even by an issuer that
        // always succeeds. (`AlwaysSucceeds` writes placeholder PEM, so the
        // on-disk file, not `load_all`, is the evidence here.)
        let cert_file = store.dir_for("a.example.com").join("cert.pem");
        let mut state = run_once(&AlwaysSucceeds, &store, &shared, &wanted, state, 60).await;
        assert!(!cert_file.exists(), "issuance should have been skipped");
        assert_eq!(state["a.example.com"].failures, 1);

        // A manual trigger clears the backoff, so the same instant succeeds.
        clear_backoff(&mut state, &wanted);
        let state = run_once(&AlwaysSucceeds, &store, &shared, &wanted, state, 60).await;
        assert!(!state.contains_key("a.example.com"));
        assert!(cert_file.exists(), "the retry should have issued");
    }

    /// A trigger fired while a pass is running must not be lost: `request`
    /// before `wait` still resolves.
    #[tokio::test]
    async fn a_trigger_fired_before_the_wait_is_not_lost() {
        let trigger = RenewalTrigger::new();
        trigger.request();
        tokio::time::timeout(Duration::from_secs(1), trigger.wait())
            .await
            .expect("a requested pass must be observed by the next wait");
    }

    /// A trigger requested after the loop is already waiting wakes it.
    #[tokio::test]
    async fn a_trigger_wakes_a_waiting_loop() {
        let trigger = RenewalTrigger::new();
        let waiter = trigger.clone();
        let handle = tokio::spawn(async move { waiter.wait().await });
        tokio::task::yield_now().await;
        trigger.request();
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("wait should have been woken")
            .unwrap();
    }

    struct AlwaysFails;

    #[async_trait::async_trait]
    impl crate::acme::CertIssuer for AlwaysFails {
        async fn issue(&self, _domain: &str) -> anyhow::Result<crate::acme::IssuedCert> {
            anyhow::bail!("nope")
        }
    }

    struct AlwaysSucceeds;

    #[async_trait::async_trait]
    impl crate::acme::CertIssuer for AlwaysSucceeds {
        async fn issue(&self, _domain: &str) -> anyhow::Result<crate::acme::IssuedCert> {
            Ok(crate::acme::IssuedCert {
                chain_pem: "not a real chain, but store.save() doesn't validate PEM content"
                    .to_string(),
                key_pem: "not a real key either".to_string(),
            })
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

    #[tokio::test]
    async fn a_successful_issuance_clears_the_domains_state() {
        let store = CertStore::new(temp_dir("clear-on-success"));
        let shared = SharedCerts::new(CertResolver::from_certs(&[]).unwrap());
        let wanted = vec!["*.dev.example.com".to_string()];

        let mut failing_state = BTreeMap::new();
        failing_state.insert(
            "*.dev.example.com".to_string(),
            DomainState {
                failures: 1,
                last_attempt: 0,
                last_error: Some("previous failure".to_string()),
            },
        );

        // failures=1, last_attempt=0 backs off only until `next_attempt(1,
        // 0) == 900`; `now` below (1000) is past that, so the pass actually
        // attempts issuance instead of being skipped for still being in
        // backoff.
        let state = run_once(
            &AlwaysSucceeds,
            &store,
            &shared,
            &wanted,
            failing_state,
            1000,
        )
        .await;
        assert!(
            !state.contains_key("*.dev.example.com"),
            "a successful issuance must clear the domain's entry, not just zero its counters"
        );
    }

    #[test]
    fn renewal_state_round_trips_through_save_and_load() {
        let store = CertStore::new(temp_dir("round-trip"));
        let mut state = BTreeMap::new();
        state.insert(
            "*.dev.example.com".to_string(),
            DomainState {
                failures: 2,
                last_attempt: 12345,
                last_error: Some("nope".to_string()),
            },
        );
        state.insert(
            "hoster.example.com".to_string(),
            DomainState {
                failures: 0,
                last_attempt: 0,
                last_error: None,
            },
        );

        save_state(&store, &state).unwrap();
        let loaded = load_state(&store);

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded["*.dev.example.com"].failures, 2);
        assert_eq!(loaded["*.dev.example.com"].last_attempt, 12345);
        assert_eq!(
            loaded["*.dev.example.com"].last_error.as_deref(),
            Some("nope")
        );
        assert_eq!(loaded["hoster.example.com"].failures, 0);
    }

    #[test]
    fn loading_state_with_no_file_present_yields_empty_state() {
        // No `save_state` call at all — this is first boot, or an upgrade
        // from a version that never wrote this file.
        let store = CertStore::new(temp_dir("no-file"));
        assert!(load_state(&store).is_empty());
    }

    #[tokio::test]
    async fn a_domain_still_in_backoff_survives_a_restart() {
        let store = CertStore::new(temp_dir("survives-restart"));
        let shared = SharedCerts::new(CertResolver::from_certs(&[]).unwrap());
        let wanted = vec!["*.dev.example.com".to_string()];

        // Fail once, then persist — the write a real `run_loop` would do
        // after every pass.
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
        save_state(&store, &state).unwrap();

        // Simulate a restart: drop the in-memory map entirely and load a
        // fresh one from disk, exactly as `run_loop` does on boot.
        drop(state);
        let restarted_state = load_state(&store);
        assert_eq!(
            restarted_state["*.dev.example.com"].failures, 1,
            "failure count must survive the simulated restart"
        );

        // One minute after the original failure — still well inside the
        // 15-minute backoff window. A process that reset backoff on restart
        // would retry here; the fix must not.
        let state_after_restart = run_once(
            &AlwaysFails,
            &store,
            &shared,
            &wanted,
            restarted_state,
            1060,
        )
        .await;
        assert_eq!(
            state_after_restart["*.dev.example.com"].failures, 1,
            "a domain still inside its persisted backoff window must be skipped after a restart"
        );
    }

    #[test]
    fn a_corrupt_state_file_loads_as_empty_state_rather_than_panicking() {
        let store = CertStore::new(temp_dir("corrupt"));
        std::fs::create_dir_all(store.root()).unwrap();
        std::fs::write(state_path(&store), b"{ this is not valid json").unwrap();

        let state = load_state(&store);
        assert!(
            state.is_empty(),
            "a corrupt state file must load as empty state, not panic or propagate an error"
        );
    }
}
