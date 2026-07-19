//! Obtaining certificates from Let's Encrypt over DNS-01.
//!
//! The challenge record for `*.dev.example.com` and for `dev.example.com` is
//! the same name, so a certificate covering both publishes two values there at
//! once — the DNS provider must append, never replace.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::proto::rr::rdata::TXT;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, Order, OrderStatus, RetryPolicy,
};

use crate::dns::DnsProvider;
use crate::settings::cert_identifiers;

/// How long to wait for a published challenge value to become visible in DNS.
const DEFAULT_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(300);

/// The TXT record name a domain's DNS-01 challenge is published at.
pub fn challenge_name(domain: &str) -> String {
    let bare = domain.strip_prefix("*.").unwrap_or(domain);
    format!("_acme-challenge.{bare}")
}

/// A certificate chain and its matching private key, both PEM-encoded.
pub struct IssuedCert {
    pub chain_pem: String,
    pub key_pem: String,
}

/// Anything that can obtain a certificate for a domain.
///
/// The renewal loop depends on this rather than on [`Issuer`] directly, so its
/// backoff and scheduling can be tested without ever contacting an ACME
/// server.
#[async_trait::async_trait]
pub trait CertIssuer: Send + Sync {
    async fn issue(&self, domain: &str) -> anyhow::Result<IssuedCert>;
}

#[async_trait::async_trait]
impl CertIssuer for Issuer {
    async fn issue(&self, domain: &str) -> anyhow::Result<IssuedCert> {
        self.issue_cert(domain).await
    }
}

/// Issues certificates from an ACME directory using DNS-01.
///
/// Defaults to the Let's Encrypt **staging** directory. Staging certificates
/// are **not trusted by browsers or other standard TLS clients** — they chain
/// to a distinct root kept only for testing — so a successful staging
/// issuance visibly proves the whole DNS-01 flow works end-to-end before
/// production is switched on. Call [`Issuer::use_production`] to opt into the
/// production directory, where mistakes are expensive: only 5 authorization
/// failures per identifier per hour, and 5 duplicate certificates per
/// identical name set per week.
pub struct Issuer {
    account_credentials_path: PathBuf,
    email: String,
    dns: Arc<dyn DnsProvider>,
    directory_url: String,
    propagation_timeout: Duration,
}

impl Issuer {
    /// An issuer against the Let's Encrypt **staging** directory (the safe
    /// default — see the type-level docs). Call [`Issuer::use_production`]
    /// once the flow has been proven here.
    pub fn new(
        account_credentials_path: PathBuf,
        email: String,
        dns: Arc<dyn DnsProvider>,
    ) -> Self {
        Self {
            account_credentials_path,
            email,
            dns,
            directory_url: LetsEncrypt::Staging.url().to_string(),
            propagation_timeout: DEFAULT_PROPAGATION_TIMEOUT,
        }
    }

    /// Opt into the Let's Encrypt **production** directory. Only call this
    /// once DNS-01 issuance has been verified end-to-end against staging
    /// (the default): production allows just 5 authorization failures per
    /// identifier per hour, and 5 duplicate certificates per identical name
    /// set per week, so a wiring mistake here is expensive to wait out.
    pub fn use_production(mut self) -> Self {
        self.directory_url = LetsEncrypt::Production.url().to_string();
        self
    }

    /// Override how long issuance waits for challenge records to propagate.
    pub fn with_propagation_timeout(mut self, timeout: Duration) -> Self {
        self.propagation_timeout = timeout;
        self
    }

    /// Obtain a certificate covering `domain` (and, for a wildcard, its parent).
    pub async fn issue_cert(&self, domain: &str) -> anyhow::Result<IssuedCert> {
        let account = self.account().await?;
        let identifiers = cert_identifiers(domain)
            .into_iter()
            .map(Identifier::Dns)
            .collect::<Vec<_>>();
        let mut order = account
            .new_order(&NewOrder::new(identifiers.as_slice()))
            .await?;

        // Every published record must be removed even when issuance fails: a
        // stale TXT value is picked up by the next attempt and fails it too.
        // The published pairs are collected outside the fallible block so the
        // cleanup below cannot be skipped by an early return.
        let mut published: Vec<(String, String)> = Vec::new();
        let result = self.run_order(&mut order, &mut published).await;

        for (name, value) in &published {
            if let Err(err) = self.dns.delete_txt(name, value).await {
                tracing::warn!(
                    %name,
                    error = %err,
                    "failed to remove ACME challenge record; it may block the next issuance"
                );
            }
        }

        result
    }

    /// The issuance steps that may fail. Kept separate from `issue` so its
    /// caller can clean up unconditionally.
    async fn run_order(
        &self,
        order: &mut Order,
        published: &mut Vec<(String, String)>,
    ) -> anyhow::Result<IssuedCert> {
        // Pass 1: publish every challenge value. A wildcard and its parent
        // share one record name, so both values must be present at that name
        // before either challenge is marked ready.
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result?;
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                other => anyhow::bail!("unexpected authorization status: {other:?}"),
            }

            let challenge = authz.challenge(ChallengeType::Dns01).ok_or_else(|| {
                anyhow::anyhow!("no dns-01 challenge offered for this identifier")
            })?;
            let name = challenge_name(&challenge.identifier().to_string());
            let value = challenge.key_authorization().dns_value();
            self.dns.upsert_txt(&name, &value).await?;
            published.push((name, value));
        }

        // Wait for real visibility. Asking Let's Encrypt to validate before the
        // record resolves burns one of the five authorization failures allowed
        // per identifier per hour, so a fixed sleep is not good enough.
        //
        // The poll must go to the zone's *authoritative* nameservers, not to a
        // recursive resolver. A recursive resolver that was asked for
        // `_acme-challenge.<domain>` before the record existed — which a failed
        // first attempt guarantees — caches the NXDOMAIN for the zone's SOA
        // minimum (1800s at Cloudflare). That outlives the 300s propagation
        // timeout here, so every retry would wait out the full timeout and
        // fail, wedging the domain indefinitely. Querying the authoritative
        // servers directly sees the record the moment it is published.
        //
        // Failing here is cheap: no challenge has been marked ready yet, so a
        // resolver problem costs no authorization attempt.
        let mut resolvers: std::collections::HashMap<String, TokioResolver> =
            std::collections::HashMap::new();
        for (name, value) in published.iter() {
            if !resolvers.contains_key(name) {
                resolvers.insert(name.clone(), authoritative_resolver(name).await?);
            }
            let resolver = &resolvers[name];
            wait_for_txt(resolver, name, value, self.propagation_timeout).await?;
        }

        // Pass 2: only now mark the challenges ready. The authorization states
        // were fetched in pass 1 and are cached on the order, so this does not
        // re-fetch them.
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result?;
            if authz.status != AuthorizationStatus::Pending {
                continue;
            }
            let mut challenge = authz.challenge(ChallengeType::Dns01).ok_or_else(|| {
                anyhow::anyhow!("no dns-01 challenge offered for this identifier")
            })?;
            challenge.set_ready().await?;
        }

        let status = order.poll_ready(&RetryPolicy::default()).await?;
        if status != OrderStatus::Ready {
            anyhow::bail!("order did not become ready: {status:?}");
        }

        // `finalize` generates a fresh key pair, builds the CSR from the
        // order's identifiers, and returns the private key as PEM.
        let key_pem = order.finalize().await?;
        let chain_pem = order.poll_certificate(&RetryPolicy::default()).await?;
        Ok(IssuedCert { chain_pem, key_pem })
    }

    /// The ACME account: restored from disk when credentials exist, otherwise
    /// registered once and persisted. Registering again on every run would
    /// walk into Let's Encrypt's new-account rate limit.
    async fn account(&self) -> anyhow::Result<Account> {
        if self.account_credentials_path.exists() {
            let raw = std::fs::read_to_string(&self.account_credentials_path)?;
            let credentials: AccountCredentials = serde_json::from_str(&raw)?;
            return Ok(Account::builder()?.from_credentials(credentials).await?);
        }

        let contact = format!("mailto:{}", self.email);
        let (account, credentials) = Account::builder()?
            .create(
                &NewAccount {
                    contact: &[contact.as_str()],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory_url.clone(),
                None,
            )
            .await?;
        write_private(
            &self.account_credentials_path,
            &serde_json::to_string_pretty(&credentials)?,
        )?;
        Ok(account)
    }
}

/// Write `contents` so that only the owner can read it. The account key is in
/// there, and it authenticates every future request for a certificate.
fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");

    // `mode(0o600)` below only takes effect when the `open` call actually
    // creates the file. A `.tmp` left behind by a crashed run — with
    // whatever permissions it happened to have — would otherwise be reused
    // as-is. Removing it first guarantees the open always creates a fresh
    // file, so the mode always applies.
    let _ = std::fs::remove_file(&tmp);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let write_result = (|| -> anyhow::Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();

    // On any failure below, remove the temp file rather than leave key
    // material sitting at a well-known path outside the intended location.
    if let Err(err) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

/// The zone names to try an NS lookup against, most specific first, when
/// hunting for the authoritative servers for `name`.
///
/// The `_acme-challenge` label is dropped before walking: asking a recursive
/// resolver about a name that does not exist yet is exactly what poisons its
/// negative cache for the name we are about to poll. The walk stops before the
/// public suffix — a single-label candidate like `com` is never useful and only
/// costs a query.
fn zone_candidates(name: &str) -> Vec<String> {
    let base = name
        .strip_prefix("_acme-challenge.")
        .unwrap_or(name)
        .trim_end_matches('.');
    let labels: Vec<&str> = base.split('.').filter(|l| !l.is_empty()).collect();
    (0..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

/// Build a resolver that queries `addrs` directly, with caching off so a
/// repeated poll always reflects what the server holds right now.
fn resolver_for(addrs: &[std::net::IpAddr]) -> anyhow::Result<TokioResolver> {
    use hickory_resolver::config::{NameServerConfig, ResolverConfig};
    use hickory_resolver::net::runtime::TokioRuntimeProvider;

    if addrs.is_empty() {
        anyhow::bail!("no nameserver addresses to query");
    }
    let config = ResolverConfig::from_parts(
        None,
        vec![],
        addrs
            .iter()
            .map(|ip| NameServerConfig::udp(*ip))
            .collect::<Vec<_>>(),
    );
    let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
    builder.options_mut().cache_size = 0;
    Ok(builder.build()?)
}

/// A resolver pointed at the authoritative nameservers for `name`'s zone.
///
/// See the call site in `run_order` for why a recursive resolver is not
/// acceptable here.
async fn authoritative_resolver(name: &str) -> anyhow::Result<TokioResolver> {
    use hickory_resolver::proto::rr::RecordType;

    let system = TokioResolver::builder_tokio()?.build()?;
    for zone in zone_candidates(name) {
        let Ok(lookup) = system.lookup(zone.clone(), RecordType::NS).await else {
            continue;
        };
        let mut hosts = Vec::new();
        for record in lookup.answers() {
            if let RData::NS(ns) = &record.data {
                hosts.push(ns.0.to_utf8());
            }
        }
        let mut addrs = Vec::new();
        for host in &hosts {
            match system.lookup_ip(host.as_str()).await {
                Ok(ips) => addrs.extend(ips.iter()),
                Err(e) => tracing::warn!(%host, error = %e, "could not resolve a nameserver"),
            }
        }
        if !addrs.is_empty() {
            tracing::debug!(%zone, servers = addrs.len(), "polling authoritative nameservers");
            return resolver_for(&addrs);
        }
    }
    anyhow::bail!(
        "could not find the authoritative nameservers for {name}; \
refusing to poll a recursive resolver, whose cached NXDOMAIN would outlive \
the propagation timeout"
    )
}

/// A TXT record's value: its character-strings joined, as RFC 1035 intends.
/// A challenge value is short enough to arrive in one chunk, but a resolver is
/// free to split it and comparing chunk-by-chunk would then never match.
fn txt_value(txt: &TXT) -> String {
    txt.txt_data
        .iter()
        .map(|chunk| String::from_utf8_lossy(chunk))
        .collect::<Vec<_>>()
        .join("")
}

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
        if let Ok(lookup) = resolver.txt_lookup(name).await
            && lookup.answers().iter().any(|record| match &record.data {
                RData::TXT(txt) => txt_value(txt) == expected,
                _ => false,
            })
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for TXT {name} to publish the challenge value");
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(15));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "hoster-acme-test-{}-{n}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn challenge_name_is_prefixed_and_stripped_of_the_wildcard() {
        assert_eq!(
            challenge_name("*.dev.example.com"),
            "_acme-challenge.dev.example.com"
        );
        assert_eq!(
            challenge_name("dev.example.com"),
            "_acme-challenge.dev.example.com"
        );
        assert_eq!(
            challenge_name("hoster.example.com"),
            "_acme-challenge.hoster.example.com"
        );
    }

    #[test]
    fn wildcard_and_parent_share_one_challenge_name() {
        // This is why the DNS provider must append rather than overwrite.
        assert_eq!(
            challenge_name("*.dev.example.com"),
            challenge_name("dev.example.com")
        );
    }

    #[test]
    fn every_identifier_of_a_wildcard_cert_uses_the_same_challenge_name() {
        let names = cert_identifiers("*.dev.example.com")
            .iter()
            .map(|id| challenge_name(id))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "_acme-challenge.dev.example.com".to_string(),
                "_acme-challenge.dev.example.com".to_string()
            ]
        );
    }

    #[test]
    fn issuer_defaults_to_staging_and_production_requires_opt_in() {
        use crate::dns::FakeDns;

        let staging = Issuer::new(
            temp_path("account-staging.json"),
            "ops@example.com".to_string(),
            Arc::new(FakeDns::new()) as Arc<dyn DnsProvider>,
        );
        assert_eq!(staging.directory_url, LetsEncrypt::Staging.url());

        let production = Issuer::new(
            temp_path("account-production.json"),
            "ops@example.com".to_string(),
            Arc::new(FakeDns::new()) as Arc<dyn DnsProvider>,
        )
        .use_production();
        assert_eq!(production.directory_url, LetsEncrypt::Production.url());
    }

    #[test]
    fn write_private_sets_0600_even_when_a_looser_tmp_file_already_exists() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("account.json");
        let tmp = path.with_extension("tmp");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&tmp);

        // Simulate a leftover temp file from a crashed run, with wide
        // permissions that must not survive into the reused file.
        std::fs::write(&tmp, b"leftover from a crashed run").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, "fresh account credentials").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "final file must be 0600, got {mode:o}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fresh account credentials"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn zone_candidates_drop_the_challenge_label_and_stop_before_the_tld() {
        assert_eq!(
            zone_candidates("_acme-challenge.dev.example.com"),
            vec![
                "dev.example.com".to_string(),
                "example.com".to_string(),
                // `com` is deliberately absent: a single-label candidate can
                // never be the zone we need and only costs a query.
            ]
        );
    }

    #[test]
    fn zone_candidates_tolerate_a_trailing_dot_and_a_bare_name() {
        assert_eq!(
            zone_candidates("_acme-challenge.example.com."),
            vec!["example.com".to_string()]
        );
        assert_eq!(zone_candidates("localhost"), Vec::<String>::new());
    }

    #[test]
    fn resolver_for_refuses_an_empty_nameserver_set() {
        // Silently falling back to the system resolver here would reintroduce
        // the cached-NXDOMAIN wedge this whole path exists to avoid.
        assert!(resolver_for(&[]).is_err());
    }

    #[test]
    fn resolver_for_builds_a_resolver_pointed_at_the_given_address() {
        use std::net::{IpAddr, Ipv4Addr};
        assert!(resolver_for(&[IpAddr::V4(Ipv4Addr::LOCALHOST)]).is_ok());
    }

    #[tokio::test]
    async fn wait_for_txt_gives_up_at_the_deadline() {
        // A nameserver on loopback that answers nothing: the lookup fails, the
        // deadline passes, and the wait reports a timeout rather than hanging
        // or claiming the record is present. No external network involved.
        use hickory_resolver::config::{NameServerConfig, ResolverConfig};
        use hickory_resolver::net::runtime::TokioRuntimeProvider;
        use std::net::{IpAddr, Ipv4Addr};

        let config = ResolverConfig::from_parts(
            None,
            vec![],
            vec![NameServerConfig::udp(IpAddr::V4(Ipv4Addr::LOCALHOST))],
        );
        let mut builder =
            TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
        builder.options_mut().timeout = Duration::from_millis(50);
        builder.options_mut().attempts = 0;
        let resolver = builder.build().unwrap();

        let err = wait_for_txt(
            &resolver,
            "_acme-challenge.dev.example.com",
            "expected-value",
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
