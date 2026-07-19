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
use std::collections::BTreeMap;
use std::sync::Mutex;

#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Publish `value` as a TXT record at `name`, keeping existing values.
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Remove exactly `value` at `name`, keeping other values.
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Ensure the A record at `name` resolves to `ip`, replacing any existing
    /// A value(s) at that exact name. Unlike TXT, an A record has one value.
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()>;
    /// Remove the A record at `name`. A missing record is not an error.
    async fn delete_a(&self, name: &str) -> anyhow::Result<()>;
}

/// In-memory `DnsProvider` for tests.
#[derive(Default)]
pub struct FakeDns {
    records: Mutex<BTreeMap<String, Vec<String>>>,
    a_records: Mutex<BTreeMap<String, String>>,
}

impl FakeDns {
    pub fn new() -> Self {
        Self::default()
    }

    /// The TXT values currently published at `name` — for test assertions.
    pub fn values(&self, name: &str) -> Vec<String> {
        self.records
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// The A value currently published at `name` — for test assertions.
    pub fn a_value(&self, name: &str) -> Option<String> {
        self.a_records.lock().unwrap().get(name).cloned()
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

    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        self.a_records
            .lock()
            .unwrap()
            .insert(name.to_string(), ip.to_string());
        Ok(())
    }

    async fn delete_a(&self, name: &str) -> anyhow::Result<()> {
        self.a_records.lock().unwrap().remove(name);
        Ok(())
    }
}

use serde::Deserialize;
use serde::de::DeserializeOwned;

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
struct CfZone {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct CfRecord {
    id: String,
    content: String,
}

#[derive(Deserialize)]
struct CfApiError {
    code: i64,
    message: String,
}

/// Cloudflare's v4 response envelope. Cloudflare can answer with HTTP 200
/// and `success: false` for validation-type failures (e.g. bad record
/// content) — an outcome `error_for_status()` never sees, since the HTTP
/// status itself is fine. Every call must deserialize into this and check
/// `success` explicitly, or a rejected write silently reports as `Ok(())`.
#[derive(Deserialize)]
struct CfEnvelope<T> {
    success: bool,
    #[serde(default)]
    errors: Vec<CfApiError>,
    // `Option<T>` fields are optional-by-default in serde (a missing key
    // deserializes as `None`), so no `#[serde(default)]` here — adding one
    // would make serde's derive require `T: Default`, which callers using
    // `CfZone`/`CfRecord` don't provide.
    result: Option<T>,
}

impl<T> CfEnvelope<T> {
    fn into_result(self) -> anyhow::Result<T> {
        if !self.success {
            let detail = if self.errors.is_empty() {
                "Cloudflare returned success: false with no error detail".to_string()
            } else {
                self.errors
                    .iter()
                    .map(|e| format!("{} (code {})", e.message, e.code))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            anyhow::bail!("Cloudflare API call failed: {detail}");
        }
        self.result
            .ok_or_else(|| anyhow::anyhow!("Cloudflare API reported success with no result"))
    }
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

    /// GET `url`, decode the Cloudflare envelope, and surface `success: false`
    /// as an error carrying Cloudflare's own error text (never the token —
    /// the token only ever goes out in the `Authorization` header).
    async fn cf_get<T: DeserializeOwned>(&self, url: String) -> anyhow::Result<T> {
        let env: CfEnvelope<T> = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        env.into_result()
    }

    /// POST `body` to `url` and check the envelope the same way `cf_get` does.
    async fn cf_post<T: DeserializeOwned>(
        &self,
        url: String,
        body: serde_json::Value,
    ) -> anyhow::Result<T> {
        let env: CfEnvelope<T> = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        env.into_result()
    }

    /// PUT `body` to `url` and check the envelope the same way `cf_get` does.
    async fn cf_put<T: DeserializeOwned>(
        &self,
        url: String,
        body: serde_json::Value,
    ) -> anyhow::Result<T> {
        let env: CfEnvelope<T> = self
            .http
            .put(url)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        env.into_result()
    }

    /// DELETE `url` and check the envelope the same way `cf_get` does.
    async fn cf_delete(&self, url: String) -> anyhow::Result<()> {
        let env: CfEnvelope<serde_json::Value> = self
            .http
            .delete(url)
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        env.into_result().map(|_| ())
    }

    /// The zone owning `name`: the longest zone name that is a suffix of it.
    async fn zone_id(&self, name: &str) -> anyhow::Result<String> {
        if let Some(hit) = self.zone_cache.lock().unwrap().get(name).cloned() {
            return Ok(hit);
        }
        let url = format!("{}/zones", self.base_url);
        let zones: Vec<CfZone> = self.cf_get(url).await?;
        let best = zones
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

    async fn find_record(
        &self,
        zone: &str,
        name: &str,
        value: &str,
    ) -> anyhow::Result<Option<String>> {
        let url = format!(
            "{}/zones/{zone}/dns_records?type=TXT&name={}",
            self.base_url,
            urlencoding::encode(name)
        );
        let records: Vec<CfRecord> = self.cf_get(url).await?;
        Ok(records
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
        let _created: serde_json::Value = self
            .cf_post(
                url,
                serde_json::json!({
                    "type": "TXT",
                    "name": name,
                    "content": value,
                    "ttl": 60,
                }),
            )
            .await?;
        Ok(())
    }

    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let zone = self.zone_id(name).await?;
        let Some(id) = self.find_record(&zone, name, value).await? else {
            return Ok(());
        };
        let url = format!("{}/zones/{zone}/dns_records/{id}", self.base_url);
        self.cf_delete(url).await
    }

    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        let zone = self.zone_id(name).await?;
        let url = format!(
            "{}/zones/{zone}/dns_records?type=A&name={}",
            self.base_url,
            urlencoding::encode(name)
        );
        let existing: Vec<CfRecord> = self.cf_get(url).await?;
        let body = serde_json::json!({ "type": "A", "name": name, "content": ip, "ttl": 60 });
        if let Some(rec) = existing.into_iter().next() {
            let url = format!("{}/zones/{zone}/dns_records/{}", self.base_url, rec.id);
            let _updated: serde_json::Value = self.cf_put(url, body).await?;
        } else {
            let url = format!("{}/zones/{zone}/dns_records", self.base_url);
            let _created: serde_json::Value = self.cf_post(url, body).await?;
        }
        Ok(())
    }

    async fn delete_a(&self, name: &str) -> anyhow::Result<()> {
        let zone = self.zone_id(name).await?;
        let url = format!(
            "{}/zones/{zone}/dns_records?type=A&name={}",
            self.base_url,
            urlencoding::encode(name)
        );
        let existing: Vec<CfRecord> = self.cf_get(url).await?;
        let Some(rec) = existing.into_iter().next() else {
            return Ok(());
        };
        let url = format!("{}/zones/{zone}/dns_records/{}", self.base_url, rec.id);
        self.cf_delete(url).await
    }
}

const HETZNER_API: &str = "https://dns.hetzner.com/api/v1";

/// Hetzner DNS over its v1 API. Record `name` is zone-relative (unlike
/// Cloudflare's fully-qualified names), so every call converts the
/// fully-qualified name crossing the trait via [`HetznerProvider::relative`].
pub struct HetznerProvider {
    token: String,
    base_url: String,
    http: reqwest::Client,
    zone_cache: Mutex<BTreeMap<String, (String, String)>>, // name -> (zone_id, zone_name)
}

#[derive(Deserialize)]
struct HzZone {
    id: String,
    name: String,
}
#[derive(Deserialize)]
struct HzZones {
    zones: Vec<HzZone>,
}
#[derive(Deserialize)]
struct HzRecord {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    value: String,
}
#[derive(Deserialize)]
struct HzRecords {
    records: Vec<HzRecord>,
}

impl HetznerProvider {
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, HETZNER_API.to_string())
    }

    /// Construct against an arbitrary base URL, so tests can point at a local
    /// mock server instead of Hetzner.
    pub fn with_base_url(token: String, base_url: String) -> Self {
        HetznerProvider {
            token,
            base_url,
            http: reqwest::Client::new(),
            zone_cache: Mutex::new(BTreeMap::new()),
        }
    }

    /// Attach Hetzner's custom auth header (not bearer auth).
    fn req(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        r.header("Auth-API-Token", &self.token)
    }

    /// The zone owning `name`: the longest zone name that is a suffix of it.
    async fn zone(&self, name: &str) -> anyhow::Result<(String, String)> {
        if let Some(hit) = self.zone_cache.lock().unwrap().get(name).cloned() {
            return Ok(hit);
        }
        let url = format!("{}/zones", self.base_url);
        let zones: HzZones = self
            .req(self.http.get(url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let best = zones
            .zones
            .into_iter()
            .filter(|z| name == z.name || name.ends_with(&format!(".{}", z.name)))
            .max_by_key(|z| z.name.len())
            .ok_or_else(|| anyhow::anyhow!("no Hetzner zone found for {name}"))?;
        let out = (best.id.clone(), best.name.clone());
        self.zone_cache
            .lock()
            .unwrap()
            .insert(name.to_string(), out.clone());
        Ok(out)
    }

    /// Convert a fully-qualified `name` to Hetzner's zone-relative form.
    fn relative(name: &str, zone: &str) -> String {
        if name == zone {
            return "@".to_string();
        }
        name.strip_suffix(&format!(".{zone}"))
            .unwrap_or(name)
            .to_string()
    }

    async fn records(&self, zone_id: &str) -> anyhow::Result<Vec<HzRecord>> {
        let url = format!("{}/records?zone_id={zone_id}", self.base_url);
        let recs: HzRecords = self
            .req(self.http.get(url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(recs.records)
    }
}

#[async_trait]
impl DnsProvider for HetznerProvider {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        // TXT appends: only create if this exact value is absent.
        let quoted = format!("\"{value}\"");
        let exists = self
            .records(&zid)
            .await?
            .into_iter()
            .any(|r| r.kind == "TXT" && r.name == rel && r.value.trim_matches('"') == value);
        if exists {
            return Ok(());
        }
        let url = format!("{}/records", self.base_url);
        let body = serde_json::json!({ "zone_id": zid, "type": "TXT", "name": rel, "value": quoted, "ttl": 60 });
        self.req(self.http.post(url))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        let Some(rec) = self
            .records(&zid)
            .await?
            .into_iter()
            .find(|r| r.kind == "TXT" && r.name == rel && r.value.trim_matches('"') == value)
        else {
            return Ok(());
        };
        let url = format!("{}/records/{}", self.base_url, rec.id);
        self.req(self.http.delete(url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        let existing = self
            .records(&zid)
            .await?
            .into_iter()
            .find(|r| r.kind == "A" && r.name == rel);
        let body =
            serde_json::json!({ "zone_id": zid, "type": "A", "name": rel, "value": ip, "ttl": 60 });
        match existing {
            Some(rec) => {
                let url = format!("{}/records/{}", self.base_url, rec.id);
                self.req(self.http.put(url))
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?;
            }
            None => {
                let url = format!("{}/records", self.base_url);
                self.req(self.http.post(url))
                    .json(&body)
                    .send()
                    .await?
                    .error_for_status()?;
            }
        }
        Ok(())
    }

    async fn delete_a(&self, name: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        let Some(rec) = self
            .records(&zid)
            .await?
            .into_iter()
            .find(|r| r.kind == "A" && r.name == rel)
        else {
            return Ok(());
        };
        let url = format!("{}/records/{}", self.base_url, rec.id);
        self.req(self.http.delete(url))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

const NAMECHEAP_API: &str = "https://api.namecheap.com/xml.response";

/// Namecheap DNS over its XML API. There is no per-record endpoint: `getHosts`
/// returns the entire host-record set for a domain and `setHosts` replaces it
/// wholesale. Every write here is therefore read-merge-write — fetch the full
/// set, change or add exactly one record, and send the full set back.
/// Dropping any other record in that round trip is silent data loss for
/// whatever else is published on the domain, so every mutating call preserves
/// every record it doesn't touch.
pub struct NamecheapProvider {
    api_user: String,
    api_key: String,
    username: String,
    client_ip: String,
    base_url: String,
    http: reqwest::Client,
}

// Deliberately no `#[derive(Debug)]` here (matching `HetznerProvider` and
// `CloudflareProvider` above): api_key must never be printable via `{:?}`,
// and the simplest way to guarantee that is to not implement Debug at all.
#[derive(Clone)]
struct NcHost {
    name: String,
    kind: String,
    address: String,
    mx_pref: Option<String>,
    ttl: String,
}

impl NamecheapProvider {
    pub fn new(api_user: String, api_key: String, username: String, client_ip: String) -> Self {
        Self::with_base_url(api_user, api_key, username, client_ip, NAMECHEAP_API.to_string())
    }

    /// Construct against an arbitrary base URL, so tests can point at a local
    /// mock server instead of Namecheap.
    pub fn with_base_url(
        api_user: String,
        api_key: String,
        username: String,
        client_ip: String,
        base_url: String,
    ) -> Self {
        NamecheapProvider {
            api_user,
            api_key,
            username,
            client_ip,
            base_url,
            http: reqwest::Client::new(),
        }
    }

    /// Split a fully-qualified name into (host, sld, tld). Assumes a two-label
    /// registrable domain (`example.com`); does not handle multi-label public
    /// suffixes like `co.uk` — a documented limitation.
    fn split(name: &str) -> anyhow::Result<(String, String, String)> {
        let labels: Vec<&str> = name.split('.').collect();
        if labels.len() < 2 {
            anyhow::bail!("name {name} is not under a registrable domain");
        }
        let tld = labels[labels.len() - 1].to_string();
        let sld = labels[labels.len() - 2].to_string();
        let host = if labels.len() == 2 {
            "@".to_string()
        } else {
            labels[..labels.len() - 2].join(".")
        };
        Ok((host, sld, tld))
    }

    fn base_query(&self, command: &str, sld: &str, tld: &str) -> Vec<(String, String)> {
        vec![
            ("ApiUser".into(), self.api_user.clone()),
            ("ApiKey".into(), self.api_key.clone()),
            ("UserName".into(), self.username.clone()),
            ("ClientIp".into(), self.client_ip.clone()),
            ("Command".into(), command.into()),
            ("SLD".into(), sld.into()),
            ("TLD".into(), tld.into()),
        ]
    }

    async fn get_hosts(&self, sld: &str, tld: &str) -> anyhow::Result<Vec<NcHost>> {
        let q = self.base_query("namecheap.domains.dns.getHosts", sld, tld);
        // The api_key rides in the query string (Namecheap has no header auth),
        // so strip the URL from any transport/status error — reqwest would
        // otherwise attach `...ApiKey=<secret>...` to the error's Display.
        let resp = self
            .http
            .get(&self.base_url)
            .query(&q)
            .send()
            .await
            .map_err(|e| e.without_url())?
            .error_for_status()
            .map_err(|e| e.without_url())?
            .text()
            .await?;
        Self::check_status(&resp)?;
        Ok(Self::parse_hosts(&resp))
    }

    async fn set_hosts(&self, sld: &str, tld: &str, hosts: &[NcHost]) -> anyhow::Result<()> {
        let mut q = self.base_query("namecheap.domains.dns.setHosts", sld, tld);
        for (i, h) in hosts.iter().enumerate() {
            let n = i + 1;
            q.push((format!("HostName{n}"), h.name.clone()));
            q.push((format!("RecordType{n}"), h.kind.clone()));
            q.push((format!("Address{n}"), h.address.clone()));
            q.push((format!("TTL{n}"), h.ttl.clone()));
            if let Some(p) = &h.mx_pref {
                q.push((format!("MXPref{n}"), p.clone()));
            }
        }
        let resp = self
            .http
            .get(&self.base_url)
            .query(&q)
            .send()
            .await
            .map_err(|e| e.without_url())?
            .error_for_status()
            .map_err(|e| e.without_url())?
            .text()
            .await?;
        Self::check_status(&resp)?;
        Ok(())
    }

    fn check_status(xml: &str) -> anyhow::Result<()> {
        // Namecheap answers HTTP 200 with Status="ERROR" and an <Errors> block.
        if xml.contains("Status=\"ERROR\"") || xml.contains("Status=\"error\"") {
            let msg = Self::first_error(xml).unwrap_or_else(|| "Namecheap API error".into());
            anyhow::bail!("Namecheap API call failed: {msg}");
        }
        Ok(())
    }

    /// Parse the text of the first `<Error>` element, without ever touching
    /// (or being able to leak) the api_key — this only ever sees Namecheap's
    /// response body.
    fn first_error(xml: &str) -> Option<String> {
        use quick_xml::Reader;
        use quick_xml::events::Event;
        let mut r = Reader::from_str(xml);
        let mut in_error = false;
        loop {
            match r.read_event() {
                Ok(Event::Start(e)) if e.name().as_ref() == b"Error" => in_error = true,
                Ok(Event::Text(t)) if in_error => return t.unescape().ok().map(|s| s.into_owned()),
                Ok(Event::Eof) | Err(_) => return None,
                _ => {}
            }
        }
    }

    fn parse_hosts(xml: &str) -> Vec<NcHost> {
        use quick_xml::Reader;
        use quick_xml::events::Event;
        let mut r = Reader::from_str(xml);
        let mut out = Vec::new();
        loop {
            match r.read_event() {
                Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"host" => {
                    let mut name = String::new();
                    let mut kind = String::new();
                    let mut address = String::new();
                    let mut ttl = "1800".to_string();
                    let mut mx_pref = None;
                    for a in e.attributes().flatten() {
                        let v = a.unescape_value().unwrap_or_default().into_owned();
                        match a.key.as_ref() {
                            b"Name" => name = v,
                            b"Type" => kind = v,
                            b"Address" => address = v,
                            b"TTL" => ttl = v,
                            b"MXPref" => mx_pref = Some(v),
                            _ => {}
                        }
                    }
                    out.push(NcHost { name, kind, address, mx_pref, ttl });
                }
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        out
    }

    /// Merge one record into the full host set: replace a same-(name,type)
    /// record's address, else append. Never drops other records.
    fn merge(mut hosts: Vec<NcHost>, host: &str, kind: &str, address: &str) -> Vec<NcHost> {
        if let Some(h) = hosts.iter_mut().find(|h| h.name == host && h.kind == kind) {
            h.address = address.to_string();
        } else {
            hosts.push(NcHost {
                name: host.into(),
                kind: kind.into(),
                address: address.into(),
                mx_pref: None,
                ttl: "60".into(),
            });
        }
        hosts
    }
}

#[async_trait]
impl DnsProvider for NamecheapProvider {
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        let (host, sld, tld) = Self::split(name)?;
        let hosts = self.get_hosts(&sld, &tld).await?;
        let merged = Self::merge(hosts, &host, "A", ip);
        self.set_hosts(&sld, &tld, &merged).await
    }

    async fn delete_a(&self, name: &str) -> anyhow::Result<()> {
        let (host, sld, tld) = Self::split(name)?;
        let mut hosts = self.get_hosts(&sld, &tld).await?;
        let before = hosts.len();
        hosts.retain(|h| !(h.name == host && h.kind == "A"));
        if hosts.len() == before {
            return Ok(());
        }
        self.set_hosts(&sld, &tld, &hosts).await
    }

    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (host, sld, tld) = Self::split(name)?;
        let mut hosts = self.get_hosts(&sld, &tld).await?;
        if hosts
            .iter()
            .any(|h| h.name == host && h.kind == "TXT" && h.address == value)
        {
            return Ok(());
        }
        hosts.push(NcHost {
            name: host,
            kind: "TXT".into(),
            address: value.into(),
            mx_pref: None,
            ttl: "60".into(),
        });
        self.set_hosts(&sld, &tld, &hosts).await
    }

    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (host, sld, tld) = Self::split(name)?;
        let mut hosts = self.get_hosts(&sld, &tld).await?;
        let before = hosts.len();
        hosts.retain(|h| !(h.name == host && h.kind == "TXT" && h.address == value));
        if hosts.len() == before {
            return Ok(());
        }
        self.set_hosts(&sld, &tld, &hosts).await
    }
}

/// The "backup" provider: hoster manages no DNS. Each call logs the record the
/// operator must create themselves and returns Ok.
#[derive(Default)]
pub struct ManualProvider;
impl ManualProvider { pub fn new() -> Self { Self } }

#[async_trait]
impl DnsProvider for ManualProvider {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        tracing::info!(record.name = %name, record.value = %value, "manual DNS: create TXT yourself");
        Ok(())
    }
    async fn delete_txt(&self, _name: &str, _value: &str) -> anyhow::Result<()> { Ok(()) }
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        tracing::info!(record.name = %name, record.ip = %ip, "manual DNS: create A record yourself");
        Ok(())
    }
    async fn delete_a(&self, _name: &str) -> anyhow::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_appends_values_and_keeps_the_others() {
        let dns = FakeDns::new();
        dns.upsert_txt("_acme-challenge.dev.example.com", "one")
            .await
            .unwrap();
        dns.upsert_txt("_acme-challenge.dev.example.com", "two")
            .await
            .unwrap();
        let mut got = dns.values("_acme-challenge.dev.example.com");
        got.sort();
        assert_eq!(got, vec!["one".to_string(), "two".to_string()]);
    }

    #[tokio::test]
    async fn fake_deletes_only_the_named_value() {
        let dns = FakeDns::new();
        dns.upsert_txt("_acme-challenge.dev.example.com", "one")
            .await
            .unwrap();
        dns.upsert_txt("_acme-challenge.dev.example.com", "two")
            .await
            .unwrap();
        dns.delete_txt("_acme-challenge.dev.example.com", "one")
            .await
            .unwrap();
        assert_eq!(
            dns.values("_acme-challenge.dev.example.com"),
            vec!["two".to_string()]
        );
    }

    #[tokio::test]
    async fn fake_delete_of_a_missing_value_is_not_an_error() {
        let dns = FakeDns::new();
        dns.delete_txt("_acme-challenge.dev.example.com", "nope")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn fake_upsert_a_replaces_rather_than_appends() {
        let dns = FakeDns::new();
        dns.upsert_a("*.dev.example.com", "1.1.1.1").await.unwrap();
        dns.upsert_a("*.dev.example.com", "2.2.2.2").await.unwrap();
        assert_eq!(dns.a_value("*.dev.example.com").as_deref(), Some("2.2.2.2"));
    }

    #[tokio::test]
    async fn fake_delete_a_removes_the_record() {
        let dns = FakeDns::new();
        dns.upsert_a("*.dev.example.com", "1.1.1.1").await.unwrap();
        dns.delete_a("*.dev.example.com").await.unwrap();
        assert_eq!(dns.a_value("*.dev.example.com"), None);
    }

    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    /// A one-shot HTTP server returning canned JSON, recording the requests it
    /// received. Enough to assert request shape without touching the network.
    async fn mock_server(
        responses: Vec<(u16, String)>,
    ) -> (
        SocketAddr,
        std::sync::Arc<Mutex<Vec<(String, String, String)>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
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
        let (addr, seen) =
            mock_server(vec![(200, zones.to_string()), (200, created.to_string())]).await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        cf.upsert_txt("_acme-challenge.dev.example.com", "val1")
            .await
            .unwrap();

        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "expected a zone lookup then a record create");
        assert!(
            reqs[1].2.contains("_acme-challenge.dev.example.com"),
            "record must be created with the fully-qualified name, got: {}",
            reqs[1].2
        );
        assert!(reqs[1].2.contains("val1"));
        assert_eq!(reqs[1].0, "POST");
    }

    #[tokio::test]
    async fn cloudflare_upsert_appends_rather_than_replaces() {
        let zones = r#"{"success":true,"result":[{"id":"zone123","name":"example.com"}]}"#;
        let created = r#"{"success":true,"result":{"id":"rec1"}}"#;
        let (addr, seen) = mock_server(vec![
            (200, zones.to_string()),
            (200, created.to_string()),
            (200, created.to_string()),
        ])
        .await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        cf.upsert_txt("_acme-challenge.dev.example.com", "one")
            .await
            .unwrap();
        cf.upsert_txt("_acme-challenge.dev.example.com", "two")
            .await
            .unwrap();

        let reqs = seen.lock().unwrap().clone();
        assert_eq!(
            reqs.len(),
            3,
            "expected one zone lookup then two record creates: {reqs:?}"
        );
        let posts: Vec<_> = reqs.iter().filter(|(m, _, _)| m == "POST").collect();
        assert_eq!(
            posts.len(),
            2,
            "each upsert should issue its own record-creating POST: {reqs:?}"
        );
        assert!(
            reqs.iter()
                .all(|(m, _, _)| m != "PUT" && m != "PATCH" && m != "DELETE"),
            "upsert must never PUT/PATCH/DELETE an existing record: {reqs:?}"
        );
        assert!(posts[0].2.contains("one"));
        assert!(posts[1].2.contains("two"));
    }

    #[tokio::test]
    async fn cloudflare_delete_removes_only_the_matching_record() {
        let zones = r#"{"success":true,"result":[{"id":"zone123","name":"example.com"}]}"#;
        let records = r#"{"success":true,"result":[
            {"id":"recA","content":"one"},
            {"id":"recB","content":"two"},
            {"id":"recC","content":"three"}
        ]}"#;
        let deleted = r#"{"success":true,"result":{"id":"recB"}}"#;
        let (addr, seen) = mock_server(vec![
            (200, zones.to_string()),
            (200, records.to_string()),
            (200, deleted.to_string()),
        ])
        .await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        cf.delete_txt("_acme-challenge.dev.example.com", "two")
            .await
            .unwrap();

        let reqs = seen.lock().unwrap().clone();
        assert_eq!(
            reqs.len(),
            3,
            "expected zone lookup, record lookup, then delete: {reqs:?}"
        );
        assert_eq!(reqs[2].0, "DELETE");
        assert!(
            reqs[2].1.ends_with("/dns_records/recB"),
            "must delete the record matching the requested value, got: {}",
            reqs[2].1
        );
    }

    #[tokio::test]
    async fn cloudflare_delete_of_a_missing_value_issues_no_delete() {
        let zones = r#"{"success":true,"result":[{"id":"zone123","name":"example.com"}]}"#;
        let records = r#"{"success":true,"result":[{"id":"recA","content":"one"}]}"#;
        let (addr, seen) =
            mock_server(vec![(200, zones.to_string()), (200, records.to_string())]).await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        cf.delete_txt("_acme-challenge.dev.example.com", "nope")
            .await
            .unwrap();

        let reqs = seen.lock().unwrap().clone();
        assert_eq!(
            reqs.len(),
            2,
            "a missing value must not trigger any delete request: {reqs:?}"
        );
        assert!(reqs.iter().all(|(m, _, _)| m != "DELETE"));
    }

    #[tokio::test]
    async fn cloudflare_zone_lookup_picks_the_longest_matching_suffix() {
        let zones = r#"{"success":true,"result":[
            {"id":"zoneRoot","name":"example.com"},
            {"id":"zoneDev","name":"dev.example.com"}
        ]}"#;
        let (addr, _seen) = mock_server(vec![(200, zones.to_string())]).await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        let zone = cf.zone_id("_acme-challenge.dev.example.com").await.unwrap();
        assert_eq!(zone, "zoneDev");
    }

    #[tokio::test]
    async fn cloudflare_zone_lookup_errors_when_no_zone_matches() {
        let zones = r#"{"success":true,"result":[{"id":"zoneRoot","name":"example.com"}]}"#;
        let (addr, _seen) = mock_server(vec![(200, zones.to_string())]).await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        let err = cf.zone_id("_acme-challenge.other.org").await.unwrap_err();
        assert!(
            err.to_string().contains("other.org"),
            "error should name the record it couldn't place: {err}"
        );
    }

    #[tokio::test]
    async fn cloudflare_upsert_surfaces_api_level_failure_despite_http_200() {
        let zones = r#"{"success":true,"result":[{"id":"zone123","name":"example.com"}]}"#;
        let failure = r#"{"success":false,"errors":[{"code":9109,"message":"Invalid TXT record content"}],"result":null}"#;
        let (addr, _seen) =
            mock_server(vec![(200, zones.to_string()), (200, failure.to_string())]).await;

        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        let err = cf
            .upsert_txt("_acme-challenge.dev.example.com", "val1")
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("Invalid TXT record content"),
            "error should surface Cloudflare's own error message: {err}"
        );
        assert!(
            !err.to_string().contains("tok"),
            "error must not leak the API token: {err}"
        );
    }

    #[tokio::test]
    async fn cloudflare_upsert_a_creates_then_updates() {
        // `zone_id` caches by name (see `zone_cache`), and both calls below
        // use the same fully-qualified name, so only the first upsert issues
        // a zone lookup: zones, [A-lookup, create], [A-lookup, update] — 5
        // requests, not 6.
        let zones = r#"{"success":true,"result":[{"id":"z","name":"example.com"}]}"#;
        let none = r#"{"success":true,"result":[]}"#;
        let created = r#"{"success":true,"result":{"id":"rec1"}}"#;
        let one = r#"{"success":true,"result":[{"id":"rec1","content":"1.1.1.1"}]}"#;
        let updated = r#"{"success":true,"result":{"id":"rec1"}}"#;
        let (addr, seen) = mock_server(vec![
            (200, zones.into()),
            (200, none.into()),
            (200, created.into()),
            (200, one.into()),
            (200, updated.into()),
        ])
        .await;
        let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
        cf.upsert_a("*.dev.example.com", "1.1.1.1").await.unwrap();
        cf.upsert_a("*.dev.example.com", "2.2.2.2").await.unwrap();
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 5, "expected zone lookup + 2x(A-lookup+write): {reqs:?}");
        assert_eq!(reqs[2].0, "POST", "first upsert with no record must POST");
        assert_eq!(reqs[4].0, "PUT", "second upsert must PUT the existing record");
        assert!(reqs[4].2.contains("2.2.2.2"));
    }

    #[tokio::test]
    async fn hetzner_upsert_a_uses_zone_relative_name_and_token_header() {
        let zones = r#"{"zones":[{"id":"z1","name":"example.com"}]}"#;
        let none = r#"{"records":[]}"#;
        let created = r#"{"record":{"id":"r1"}}"#;
        let (addr, seen) = mock_server(vec![
            (200, zones.into()),
            (200, none.into()),
            (200, created.into()),
        ])
        .await;
        let h = HetznerProvider::with_base_url("tok".into(), format!("http://{addr}"));
        h.upsert_a("*.dev.example.com", "1.2.3.4").await.unwrap();
        let reqs = seen.lock().unwrap().clone();
        let create = reqs.last().unwrap();
        assert_eq!(create.0, "POST");
        assert!(
            create.2.contains("\"name\":\"*.dev\""),
            "zone-relative name; got {}",
            create.2
        );
        assert!(create.2.contains("1.2.3.4"));
    }

    #[tokio::test]
    async fn hetzner_relative_name_is_apex_when_name_equals_zone() {
        assert_eq!(
            HetznerProvider::relative("dev.example.com", "dev.example.com"),
            "@"
        );
        assert_eq!(
            HetznerProvider::relative("*.dev.example.com", "example.com"),
            "*.dev"
        );
        assert_eq!(
            HetznerProvider::relative("*.dev.example.com", "dev.example.com"),
            "*"
        );
    }

    #[tokio::test]
    async fn namecheap_sethosts_preserves_sibling_records() {
        // getHosts returns two unrelated hosts; our upsert must send all THREE back.
        let get = r#"<?xml version="1.0"?><ApiResponse Status="OK"><CommandResponse>
          <DomainDNSGetHostsResult>
            <host Name="@" Type="A" Address="9.9.9.9" TTL="1800"/>
            <host Name="mail" Type="MX" Address="mx.example.com" MXPref="10" TTL="1800"/>
          </DomainDNSGetHostsResult></CommandResponse></ApiResponse>"#;
        let set = r#"<?xml version="1.0"?><ApiResponse Status="OK"><CommandResponse>
          <DomainDNSSetHostsResult IsSuccess="true"/></CommandResponse></ApiResponse>"#;
        let (addr, seen) = mock_server(vec![(200, get.into()), (200, set.into())]).await;
        let nc = NamecheapProvider::with_base_url(
            "u".into(), "k".into(), "u".into(), "1.2.3.4".into(), format!("http://{addr}"));
        nc.upsert_a("*.dev.example.com", "5.6.7.8").await.unwrap();

        let reqs = seen.lock().unwrap().clone();
        let set_req = reqs.last().unwrap();
        // full record set round-trips: the two siblings AND our new record.
        assert!(set_req.1.contains("HostName1=%40") || set_req.1.contains("HostName1=@"), "apex kept: {}", set_req.1);
        assert!(set_req.1.contains("mail"), "MX sibling kept: {}", set_req.1);
        assert!(set_req.1.contains("5.6.7.8"), "new A value present: {}", set_req.1);
        assert!(set_req.1.contains(&"Address".to_string()));
    }

    #[test]
    fn namecheap_split_puts_subdomain_in_host() {
        assert_eq!(
            NamecheapProvider::split("*.dev.example.com").unwrap(),
            ("*.dev".into(), "example".into(), "com".into())
        );
        assert_eq!(
            NamecheapProvider::split("example.com").unwrap(),
            ("@".into(), "example".into(), "com".into())
        );
    }

    #[tokio::test]
    async fn namecheap_surfaces_api_error_without_leaking_key() {
        let err = r#"<?xml version="1.0"?><ApiResponse Status="ERROR">
          <Errors><Error Number="1011150">Invalid request IP</Error></Errors></ApiResponse>"#;
        let (addr, _seen) = mock_server(vec![(200, err.into())]).await;
        let nc = NamecheapProvider::with_base_url(
            "u".into(), "supersecret".into(), "u".into(), "1.2.3.4".into(), format!("http://{addr}"));
        let e = nc.upsert_a("*.dev.example.com", "5.6.7.8").await.unwrap_err();
        assert!(e.to_string().contains("Invalid request IP"), "got {e}");
        assert!(!e.to_string().contains("supersecret"), "must not leak api key: {e}");
    }

    #[tokio::test]
    async fn namecheap_http_error_does_not_leak_api_key() {
        // Namecheap carries the api_key as a query param, so a non-2xx (or any
        // transport failure) must not surface the request URL — reqwest would
        // otherwise attach `ApiKey=<secret>` to the error's Display.
        let (addr, _seen) = mock_server(vec![(500, "<oops/>".into())]).await;
        let nc = NamecheapProvider::with_base_url(
            "u".into(), "supersecret".into(), "u".into(), "1.2.3.4".into(), format!("http://{addr}"));
        let e = nc.upsert_a("*.dev.example.com", "5.6.7.8").await.unwrap_err();
        assert!(!e.to_string().contains("supersecret"), "api_key leaked in error: {e}");
    }

    #[tokio::test]
    async fn manual_provider_is_a_noop() {
        let m = ManualProvider::new();
        m.upsert_a("*.dev.example.com", "1.2.3.4").await.unwrap();
        m.delete_a("*.dev.example.com").await.unwrap();
        m.upsert_txt("_acme-challenge.dev.example.com", "v").await.unwrap();
        m.delete_txt("_acme-challenge.dev.example.com", "v").await.unwrap();
    }
}
