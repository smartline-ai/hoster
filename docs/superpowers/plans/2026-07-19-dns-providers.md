# DNS Providers & Wildcard A Records — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Hetzner, Namecheap, and Manual DNS providers alongside Cloudflare, automatically publish one wildcard `A` record per project base domain, and make the DNS provider per-project with a global default — driving both A-record management and ACME DNS-01 from the same resolved provider.

**Architecture:** Extend the existing `DnsProvider` trait (currently TXT-only) with `upsert_a`/`delete_a`. Implement four backends. Generalize the stored credential to carry either a single token (Cloudflare/Hetzner) or Namecheap's three fields. Resolve a provider per base domain (project override → global default → manual no-op) via a factory, and call it from the deploy path (wildcard A) and the renewal issuer (DNS-01 TXT).

**Tech Stack:** Rust 2024, `async-trait`, `reqwest` (rustls), `serde`/`serde_json`, `quick-xml` (new, for Namecheap's XML API), `tracing`. Tests use the in-module `mock_server` HTTP harness already in `src/dns.rs`.

## Global Constraints

- Rust edition 2024; crate name `hoster`. Do not bump the MSRV or edition.
- Secrets (`token`, `api_key`) must **never** appear in `Debug`, serialized masked views, logs, or error strings. Every task touching credentials keeps this guarantee and has a test asserting it.
- Names crossing the `DnsProvider` trait are **fully qualified** (e.g. `*.dev.example.com`, `_acme-challenge.dev.example.com`). Each provider converts to whatever its own API expects.
- `upsert_txt` **appends** (wildcard + parent publish two values at one name); `upsert_a` **replaces** (a hostname has one A value). Do not conflate them.
- New providers are constructable against an override base URL (like `CloudflareProvider::with_base_url`) so tests point at `mock_server` instead of the network.
- Provider kinds are the exact strings: `"cloudflare"`, `"hetzner"`, `"namecheap"`, `"manual"`.
- On-disk store format must stay backward-compatible with existing `{ "kind": "cloudflare", "token": "..." }` DNS configs.
- Follow existing patterns: `anyhow::Result`, `#[async_trait]`, one responsibility per file, tests in a `#[cfg(test)] mod tests` at the bottom of each module.

---

## File Structure

- `src/dns.rs` — the `DnsProvider` trait, `FakeDns`, and all four provider impls (Cloudflare, Hetzner, Namecheap, Manual) + the `build_provider` factory. Providers are small and share the `mock_server` test harness, so they stay in one module (matches the current layout).
- `src/secrets.rs` — `DnsProviderConfig` credential model, per-project DNS provider storage, and the `dns_provider_for(base)` resolver.
- `src/settings.rs` — `HOSTER_PUBLIC_IP` field on `Settings`.
- `src/main.rs` — read `HOSTER_PUBLIC_IP` from env; rewire `StoreIssuer` to resolve the provider per domain.
- `src/engine.rs` — ensure the wildcard `A` record on deploy (idempotent, non-fatal).
- `src/api.rs` — endpoints to set/clear the global default and per-project DNS provider.
- `src/ui/settings.rs` — guided setup UI (provider picker, per-kind instructions, manual-mode records, public-IP state, verify affordance).
- `Cargo.toml` — add `quick-xml`.

---

### Task 1: Extend `DnsProvider` with A-record ops (trait + FakeDns + Cloudflare)

**Files:**
- Modify: `src/dns.rs` (trait at 16-22; `FakeDns` at 24-64; `CloudflareProvider` impl at 237-266)
- Test: `src/dns.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: existing `DnsProvider`, `FakeDns`, `CloudflareProvider`, `mock_server`.
- Produces:
  - `DnsProvider::upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()>`
  - `DnsProvider::delete_a(&self, name: &str) -> anyhow::Result<()>`
  - `FakeDns::a_value(&self, name: &str) -> Option<String>`

- [ ] **Step 1: Write the failing tests (FakeDns A semantics)**

Add to `mod tests` in `src/dns.rs`:

```rust
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
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib dns::tests::fake_upsert_a_replaces_rather_than_appends`
Expected: FAIL — `no method named upsert_a` / `a_value`.

- [ ] **Step 3: Add the trait methods**

In `src/dns.rs`, extend the trait:

```rust
#[async_trait]
pub trait DnsProvider: Send + Sync {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Ensure the A record at `name` resolves to `ip`, replacing any existing
    /// A value(s) at that exact name. Unlike TXT, an A record has one value.
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()>;
    /// Remove the A record at `name`. A missing record is not an error.
    async fn delete_a(&self, name: &str) -> anyhow::Result<()>;
}
```

- [ ] **Step 4: Implement for `FakeDns`**

Add an A map to the struct and impl:

```rust
#[derive(Default)]
pub struct FakeDns {
    records: Mutex<BTreeMap<String, Vec<String>>>,
    a_records: Mutex<BTreeMap<String, String>>,
}
```
```rust
impl FakeDns {
    pub fn a_value(&self, name: &str) -> Option<String> {
        self.a_records.lock().unwrap().get(name).cloned()
    }
}
```
Inside `impl DnsProvider for FakeDns`:
```rust
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        self.a_records.lock().unwrap().insert(name.to_string(), ip.to_string());
        Ok(())
    }
    async fn delete_a(&self, name: &str) -> anyhow::Result<()> {
        self.a_records.lock().unwrap().remove(name);
        Ok(())
    }
```

- [ ] **Step 5: Implement for `CloudflareProvider`**

Add a typed field to `CfRecord` for `type` filtering and add the A methods. Extend `CfRecord`:
```rust
#[derive(Deserialize)]
struct CfRecord {
    id: String,
    content: String,
}
```
Add a helper mirroring `find_record` but for A, then the trait methods inside `impl DnsProvider for CloudflareProvider`:
```rust
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        let zone = self.zone_id(name).await?;
        let url = format!(
            "{}/zones/{zone}/dns_records?type=A&name={}",
            self.base_url, urlencoding::encode(name)
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
            self.base_url, urlencoding::encode(name)
        );
        let existing: Vec<CfRecord> = self.cf_get(url).await?;
        let Some(rec) = existing.into_iter().next() else { return Ok(()); };
        let url = format!("{}/zones/{zone}/dns_records/{}", self.base_url, rec.id);
        self.cf_delete(url).await
    }
```
Add a `cf_put` alongside `cf_post` (identical but `.put`):
```rust
    async fn cf_put<T: DeserializeOwned>(&self, url: String, body: serde_json::Value) -> anyhow::Result<T> {
        let env: CfEnvelope<T> = self.http.put(url).bearer_auth(&self.token)
            .json(&body).send().await?.error_for_status()?.json().await?;
        env.into_result()
    }
```

- [ ] **Step 6: Add a Cloudflare A test**

```rust
#[tokio::test]
async fn cloudflare_upsert_a_creates_then_updates() {
    let zones = r#"{"success":true,"result":[{"id":"z","name":"example.com"}]}"#;
    let none = r#"{"success":true,"result":[]}"#;
    let created = r#"{"success":true,"result":{"id":"rec1"}}"#;
    let one = r#"{"success":true,"result":[{"id":"rec1","content":"1.1.1.1"}]}"#;
    let updated = r#"{"success":true,"result":{"id":"rec1"}}"#;
    let (addr, seen) = mock_server(vec![
        (200, zones.into()), (200, none.into()), (200, created.into()),
        (200, zones.into()), (200, one.into()), (200, updated.into()),
    ]).await;
    let cf = CloudflareProvider::with_base_url("tok".into(), format!("http://{addr}"));
    cf.upsert_a("*.dev.example.com", "1.1.1.1").await.unwrap();
    cf.upsert_a("*.dev.example.com", "2.2.2.2").await.unwrap();
    let reqs = seen.lock().unwrap().clone();
    assert_eq!(reqs[2].0, "POST", "first upsert with no record must POST");
    assert_eq!(reqs[5].0, "PUT", "second upsert must PUT the existing record");
    assert!(reqs[5].2.contains("2.2.2.2"));
}
```

- [ ] **Step 7: Run all dns tests**

Run: `cargo test --lib dns::`
Expected: PASS (all existing + new).

- [ ] **Step 8: Commit**

```bash
git add src/dns.rs
git commit -m "feat(dns): add A-record ops to the DnsProvider trait (FakeDns + Cloudflare)"
```

---

### Task 2: Hetzner DNS provider

**Files:**
- Modify: `src/dns.rs`
- Test: `src/dns.rs` `mod tests`

**Interfaces:**
- Consumes: `DnsProvider`, `mock_server`, `reqwest`, `serde`.
- Produces: `HetznerProvider::new(token: String) -> Self`, `HetznerProvider::with_base_url(token: String, base_url: String) -> Self`.

Hetzner DNS API notes baked into the code below: auth is the **custom header** `Auth-API-Token` (not bearer); record `name` is **zone-relative** (`*` under `dev.example.com`, or `*.dev` under `example.com`, `@` for apex); TXT `value` must be wrapped in quotes; zone is chosen by longest-suffix match like Cloudflare.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn hetzner_upsert_a_uses_zone_relative_name_and_token_header() {
    let zones = r#"{"zones":[{"id":"z1","name":"example.com"}]}"#;
    let none = r#"{"records":[]}"#;
    let created = r#"{"record":{"id":"r1"}}"#;
    let (addr, seen) = mock_server(vec![
        (200, zones.into()), (200, none.into()), (200, created.into()),
    ]).await;
    let h = HetznerProvider::with_base_url("tok".into(), format!("http://{addr}"));
    h.upsert_a("*.dev.example.com", "1.2.3.4").await.unwrap();
    let reqs = seen.lock().unwrap().clone();
    let create = reqs.last().unwrap();
    assert_eq!(create.0, "POST");
    assert!(create.2.contains("\"name\":\"*.dev\""), "zone-relative name; got {}", create.2);
    assert!(create.2.contains("1.2.3.4"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib dns::tests::hetzner_upsert_a_uses_zone_relative_name_and_token_header`
Expected: FAIL — `HetznerProvider` undefined.

- [ ] **Step 3: Implement `HetznerProvider`**

Add to `src/dns.rs`:

```rust
const HETZNER_API: &str = "https://dns.hetzner.com/api/v1";

pub struct HetznerProvider {
    token: String,
    base_url: String,
    http: reqwest::Client,
    zone_cache: Mutex<BTreeMap<String, (String, String)>>, // name -> (zone_id, zone_name)
}

#[derive(Deserialize)]
struct HzZone { id: String, name: String }
#[derive(Deserialize)]
struct HzZones { zones: Vec<HzZone> }
#[derive(Deserialize)]
struct HzRecord { id: String, #[serde(rename = "type")] kind: String, name: String, value: String }
#[derive(Deserialize)]
struct HzRecords { records: Vec<HzRecord> }

impl HetznerProvider {
    pub fn new(token: String) -> Self { Self::with_base_url(token, HETZNER_API.to_string()) }
    pub fn with_base_url(token: String, base_url: String) -> Self {
        HetznerProvider { token, base_url, http: reqwest::Client::new(), zone_cache: Mutex::new(BTreeMap::new()) }
    }

    fn req(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        r.header("Auth-API-Token", &self.token)
    }

    async fn zone(&self, name: &str) -> anyhow::Result<(String, String)> {
        if let Some(hit) = self.zone_cache.lock().unwrap().get(name).cloned() {
            return Ok(hit);
        }
        let url = format!("{}/zones", self.base_url);
        let zones: HzZones = self.req(self.http.get(url)).send().await?.error_for_status()?.json().await?;
        let best = zones.zones.into_iter()
            .filter(|z| name == z.name || name.ends_with(&format!(".{}", z.name)))
            .max_by_key(|z| z.name.len())
            .ok_or_else(|| anyhow::anyhow!("no Hetzner zone found for {name}"))?;
        let out = (best.id.clone(), best.name.clone());
        self.zone_cache.lock().unwrap().insert(name.to_string(), out.clone());
        Ok(out)
    }

    /// Convert a fully-qualified `name` to Hetzner's zone-relative form.
    fn relative(name: &str, zone: &str) -> String {
        if name == zone { return "@".to_string(); }
        name.strip_suffix(&format!(".{zone}")).unwrap_or(name).to_string()
    }

    async fn records(&self, zone_id: &str) -> anyhow::Result<Vec<HzRecord>> {
        let url = format!("{}/records?zone_id={zone_id}", self.base_url);
        let recs: HzRecords = self.req(self.http.get(url)).send().await?.error_for_status()?.json().await?;
        Ok(recs.records)
    }
}
```

Then the trait impl:

```rust
#[async_trait]
impl DnsProvider for HetznerProvider {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        // TXT appends: only create if this exact value is absent.
        let quoted = format!("\"{value}\"");
        let exists = self.records(&zid).await?.into_iter()
            .any(|r| r.kind == "TXT" && r.name == rel && r.value.trim_matches('"') == value);
        if exists { return Ok(()); }
        let url = format!("{}/records", self.base_url);
        let body = serde_json::json!({ "zone_id": zid, "type": "TXT", "name": rel, "value": quoted, "ttl": 60 });
        self.req(self.http.post(url)).json(&body).send().await?.error_for_status()?;
        Ok(())
    }
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        let Some(rec) = self.records(&zid).await?.into_iter()
            .find(|r| r.kind == "TXT" && r.name == rel && r.value.trim_matches('"') == value) else { return Ok(()); };
        let url = format!("{}/records/{}", self.base_url, rec.id);
        self.req(self.http.delete(url)).send().await?.error_for_status()?;
        Ok(())
    }
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        let existing = self.records(&zid).await?.into_iter().find(|r| r.kind == "A" && r.name == rel);
        let body = serde_json::json!({ "zone_id": zid, "type": "A", "name": rel, "value": ip, "ttl": 60 });
        match existing {
            Some(rec) => {
                let url = format!("{}/records/{}", self.base_url, rec.id);
                self.req(self.http.put(url)).json(&body).send().await?.error_for_status()?;
            }
            None => {
                let url = format!("{}/records", self.base_url);
                self.req(self.http.post(url)).json(&body).send().await?.error_for_status()?;
            }
        }
        Ok(())
    }
    async fn delete_a(&self, name: &str) -> anyhow::Result<()> {
        let (zid, zname) = self.zone(name).await?;
        let rel = Self::relative(name, &zname);
        let Some(rec) = self.records(&zid).await?.into_iter().find(|r| r.kind == "A" && r.name == rel) else { return Ok(()); };
        let url = format!("{}/records/{}", self.base_url, rec.id);
        self.req(self.http.delete(url)).send().await?.error_for_status()?;
        Ok(())
    }
}
```

- [ ] **Step 4: Add a TXT-append + relative-name test**

```rust
#[tokio::test]
async fn hetzner_relative_name_is_apex_when_name_equals_zone() {
    assert_eq!(HetznerProvider::relative("dev.example.com", "dev.example.com"), "@");
    assert_eq!(HetznerProvider::relative("*.dev.example.com", "example.com"), "*.dev");
    assert_eq!(HetznerProvider::relative("*.dev.example.com", "dev.example.com"), "*");
}
```

- [ ] **Step 5: Run**

Run: `cargo test --lib dns::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/dns.rs
git commit -m "feat(dns): add Hetzner DNS provider (A + TXT, zone-relative names)"
```

---

### Task 3: Namecheap provider (read-merge-write)

**Files:**
- Modify: `src/dns.rs`, `Cargo.toml`
- Test: `src/dns.rs` `mod tests`

**Interfaces:**
- Consumes: `DnsProvider`, `mock_server`, `reqwest`, `quick-xml`.
- Produces: `NamecheapProvider::new(api_user, api_key, username, client_ip) -> Self`, `NamecheapProvider::with_base_url(...) -> Self`.

Namecheap notes: every call is a GET to one endpoint with query params `ApiUser`, `ApiKey`, `UserName`, `ClientIp`, `Command`, plus `SLD`/`TLD`. `getHosts` returns the **entire** host set; `setHosts` **replaces** it. So writes are read-merge-write and must preserve every sibling record. The response is XML. `SLD`/`TLD` are the last two labels of the base domain; everything before them is the host `Name` (`*.dev` for `*.dev.example.com`). Multi-label public suffixes (e.g. `co.uk`) are **not** handled — documented limitation.

- [ ] **Step 1: Add the XML dependency**

In `Cargo.toml` `[dependencies]`:
```toml
quick-xml = "0.36"
```
Run: `cargo build` — Expected: compiles.

- [ ] **Step 2: Write the failing preservation test**

```rust
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
```

Note: Namecheap `setHosts` puts records in the **query string** (that's what Namecheap accepts), so assertions inspect `set_req.1` (the path/query) rather than the body.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test --lib dns::tests::namecheap_sethosts_preserves_sibling_records`
Expected: FAIL — `NamecheapProvider` undefined.

- [ ] **Step 4: Implement `NamecheapProvider`**

```rust
const NAMECHEAP_API: &str = "https://api.namecheap.com/xml.response";

pub struct NamecheapProvider {
    api_user: String,
    api_key: String,
    username: String,
    client_ip: String,
    base_url: String,
    http: reqwest::Client,
}

#[derive(Clone)]
struct NcHost { name: String, kind: String, address: String, mx_pref: Option<String>, ttl: String }

impl NamecheapProvider {
    pub fn new(api_user: String, api_key: String, username: String, client_ip: String) -> Self {
        Self::with_base_url(api_user, api_key, username, client_ip, NAMECHEAP_API.to_string())
    }
    pub fn with_base_url(api_user: String, api_key: String, username: String, client_ip: String, base_url: String) -> Self {
        NamecheapProvider { api_user, api_key, username, client_ip, base_url, http: reqwest::Client::new() }
    }

    /// Split a fully-qualified name into (host, sld, tld). Assumes a two-label
    /// registrable domain (`example.com`); does not handle multi-label TLDs.
    fn split(name: &str) -> anyhow::Result<(String, String, String)> {
        let labels: Vec<&str> = name.split('.').collect();
        if labels.len() < 2 { anyhow::bail!("name {name} is not under a registrable domain"); }
        let tld = labels[labels.len() - 1].to_string();
        let sld = labels[labels.len() - 2].to_string();
        let host = if labels.len() == 2 { "@".to_string() } else { labels[..labels.len() - 2].join(".") };
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
        let resp = self.http.get(&self.base_url).query(&q).send().await?.error_for_status()?.text().await?;
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
            if let Some(p) = &h.mx_pref { q.push((format!("MXPref{n}"), p.clone())); }
        }
        let resp = self.http.get(&self.base_url).query(&q).send().await?.error_for_status()?.text().await?;
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

    fn first_error(xml: &str) -> Option<String> { /* parse <Error ...>text</Error> with quick_xml */ 
        use quick_xml::events::Event;
        use quick_xml::Reader;
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
        use quick_xml::events::Event;
        use quick_xml::Reader;
        let mut r = Reader::from_str(xml);
        let mut out = Vec::new();
        loop {
            match r.read_event() {
                Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"host" => {
                    let mut name = String::new(); let mut kind = String::new();
                    let mut address = String::new(); let mut ttl = "1800".to_string();
                    let mut mx_pref = None;
                    for a in e.attributes().flatten() {
                        let v = a.unescape_value().unwrap_or_default().into_owned();
                        match a.key.as_ref() {
                            b"Name" => name = v, b"Type" => kind = v, b"Address" => address = v,
                            b"TTL" => ttl = v, b"MXPref" => mx_pref = Some(v), _ => {}
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
            hosts.push(NcHost { name: host.into(), kind: kind.into(), address: address.into(), mx_pref: None, ttl: "60".into() });
        }
        hosts
    }
}
```

Trait impl (A replaces; TXT appends by keeping distinct values as separate records):

```rust
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
        if hosts.len() == before { return Ok(()); }
        self.set_hosts(&sld, &tld, &hosts).await
    }
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (host, sld, tld) = Self::split(name)?;
        let mut hosts = self.get_hosts(&sld, &tld).await?;
        if hosts.iter().any(|h| h.name == host && h.kind == "TXT" && h.address == value) { return Ok(()); }
        hosts.push(NcHost { name: host, kind: "TXT".into(), address: value.into(), mx_pref: None, ttl: "60".into() });
        self.set_hosts(&sld, &tld, &hosts).await
    }
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()> {
        let (host, sld, tld) = Self::split(name)?;
        let mut hosts = self.get_hosts(&sld, &tld).await?;
        let before = hosts.len();
        hosts.retain(|h| !(h.name == host && h.kind == "TXT" && h.address == value));
        if hosts.len() == before { return Ok(()); }
        self.set_hosts(&sld, &tld, &hosts).await
    }
}
```

- [ ] **Step 5: Add a name-split test and an error-surfacing test**

```rust
#[test]
fn namecheap_split_puts_subdomain_in_host() {
    assert_eq!(NamecheapProvider::split("*.dev.example.com").unwrap(), ("*.dev".into(), "example".into(), "com".into()));
    assert_eq!(NamecheapProvider::split("example.com").unwrap(), ("@".into(), "example".into(), "com".into()));
}

#[tokio::test]
async fn namecheap_surfaces_api_error_without_leaking_key() {
    let err = r#"<?xml version="1.0"?><ApiResponse Status="ERROR">
      <Errors><Error Number="1011150">Invalid request IP</Error></Errors></ApiResponse>"#;
    let (addr, _seen) = mock_server(vec![(200, err.into())]).await;
    let nc = NamecheapProvider::with_base_url("u".into(), "supersecret".into(), "u".into(), "1.2.3.4".into(), format!("http://{addr}"));
    let e = nc.upsert_a("*.dev.example.com", "5.6.7.8").await.unwrap_err();
    assert!(e.to_string().contains("Invalid request IP"), "got {e}");
    assert!(!e.to_string().contains("supersecret"), "must not leak api key: {e}");
}
```

- [ ] **Step 6: Run**

Run: `cargo test --lib dns::`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/dns.rs Cargo.toml Cargo.lock
git commit -m "feat(dns): add Namecheap provider (read-merge-write, sibling-safe)"
```

---

### Task 4: Manual (no-op) provider

**Files:**
- Modify: `src/dns.rs`
- Test: `src/dns.rs` `mod tests`

**Interfaces:**
- Produces: `ManualProvider::new() -> Self` (also `Default`).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn manual_provider_is_a_noop() {
    let m = ManualProvider::new();
    m.upsert_a("*.dev.example.com", "1.2.3.4").await.unwrap();
    m.delete_a("*.dev.example.com").await.unwrap();
    m.upsert_txt("_acme-challenge.dev.example.com", "v").await.unwrap();
    m.delete_txt("_acme-challenge.dev.example.com", "v").await.unwrap();
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib dns::tests::manual_provider_is_a_noop`
Expected: FAIL — `ManualProvider` undefined.

- [ ] **Step 3: Implement**

```rust
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
```

- [ ] **Step 4: Run**

Run: `cargo test --lib dns::tests::manual_provider_is_a_noop`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dns.rs
git commit -m "feat(dns): add Manual no-op provider (backup mode)"
```

---

### Task 5: Generalize the stored credential model

**Files:**
- Modify: `src/secrets.rs` (`DnsProviderConfig` 76-97; `set_dns_token` 412-434; `MaskedAcme` 114-119 and its projection 451-461)
- Modify: `src/main.rs` (`StoreIssuer::issue` 52-58 — keep compiling only; full rewiring is Task 10)
- Modify: `src/api.rs` (wherever `set_dns_token` is called — keep the Cloudflare path working)
- Test: `src/secrets.rs` `mod tests`

**Interfaces:**
- Consumes: existing `AcmeConfig`, `Store`.
- Produces:
  - `DnsProviderConfig { kind: String, token: Option<String>, api_user: Option<String>, api_key: Option<String>, username: Option<String> }` (serde back-compat: legacy `{kind, token}` still deserializes).
  - `Store::set_dns_provider(&self, cfg: DnsProviderConfig) -> Result<(), String>` (validates per kind).
  - `DnsProviderConfig::validate(&self) -> Result<(), String>`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn dns_config_debug_redacts_all_secrets() {
    let cfg = DnsProviderConfig { kind: "namecheap".into(), token: None,
        api_user: Some("u".into()), api_key: Some("SECRETKEY".into()), username: Some("u".into()) };
    let shown = format!("{cfg:?}");
    assert!(!shown.contains("SECRETKEY"), "api_key leaked: {shown}");
    assert!(shown.contains("namecheap"));
}

#[test]
fn dns_config_validation_requires_kind_specific_fields() {
    let missing = DnsProviderConfig { kind: "namecheap".into(), token: None, api_user: Some("u".into()), api_key: None, username: Some("u".into()) };
    assert!(missing.validate().is_err(), "namecheap without api_key must fail");
    let ok = DnsProviderConfig { kind: "hetzner".into(), token: Some("t".into()), api_user: None, api_key: None, username: None };
    assert!(ok.validate().is_ok());
    let bad_kind = DnsProviderConfig { kind: "route53".into(), token: Some("t".into()), api_user: None, api_key: None, username: None };
    assert!(bad_kind.validate().is_err());
}

#[test]
fn legacy_cloudflare_token_still_deserializes() {
    let legacy: DnsProviderConfig = serde_json::from_str(r#"{"kind":"cloudflare","token":"cf_tok"}"#).unwrap();
    assert_eq!(legacy.kind, "cloudflare");
    assert_eq!(legacy.token.as_deref(), Some("cf_tok"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib secrets::tests::dns_config_validation_requires_kind_specific_fields`
Expected: FAIL — fields/`validate` don't exist.

- [ ] **Step 3: Replace `DnsProviderConfig`**

```rust
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsProviderConfig {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl std::fmt::Debug for DnsProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DnsProviderConfig")
            .field("kind", &self.kind)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("api_user", &self.api_user)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("username", &self.username)
            .finish()
    }
}

impl DnsProviderConfig {
    pub fn validate(&self) -> Result<(), String> {
        let need = |o: &Option<String>, f: &str| -> Result<(), String> {
            match o { Some(v) if !v.trim().is_empty() => Ok(()), _ => Err(format!("{} requires {f}", self.kind)) }
        };
        match self.kind.as_str() {
            "cloudflare" | "hetzner" => need(&self.token, "an API token"),
            "namecheap" => { need(&self.api_user, "api_user")?; need(&self.api_key, "api_key")?; need(&self.username, "username") }
            "manual" => Ok(()),
            other => Err(format!("unknown DNS provider {other:?}; supported: cloudflare, hetzner, namecheap, manual")),
        }
    }
}
```

- [ ] **Step 4: Replace `set_dns_token` with `set_dns_provider`**

```rust
    /// Set the global default DNS provider credentials. Requires the ACME email
    /// to be set first (issuance needs it).
    pub fn set_dns_provider(&self, cfg: DnsProviderConfig) -> Result<(), String> {
        cfg.validate()?;
        let mut data = self.data.lock().unwrap();
        let acme = data.acme.as_mut().ok_or("set the ACME email before a DNS provider")?;
        acme.provider = Some(cfg);
        self.persist(&data).map_err(|e| e.to_string())
    }
```
Keep `delete_dns_token` as-is (renaming optional). Update `MaskedAcme` projection at 451-461 so `provider_kind` reads `a.provider.as_ref().map(|p| p.kind.clone())` (already does) and `token_set` becomes `a.provider.is_some()` (already does) — no change needed beyond confirming it compiles.

- [ ] **Step 5: Fix the two call sites so the crate compiles**

`src/api.rs`: the handler that called `store.set_dns_token(kind, token)` now builds a `DnsProviderConfig` and calls `set_dns_provider`. Minimal change to keep the existing Cloudflare form working (the full multi-provider API is Task 11):
```rust
    let cfg = hoster::secrets::DnsProviderConfig {
        kind: body.kind.clone(),
        token: Some(body.token.clone()),
        api_user: None, api_key: None, username: None,
    };
    store.set_dns_provider(cfg).map_err(/* existing error mapping */)?;
```
`src/main.rs` `StoreIssuer::issue`: it reads `provider.token`; make it compile against the new `Option<String>` by unwrapping for the Cloudflare-only path (Task 10 replaces this whole block):
```rust
    let token = provider.token.clone().ok_or_else(|| anyhow::anyhow!("cloudflare provider missing token"))?;
    let dns: Arc<dyn DnsProvider> = Arc::new(CloudflareProvider::new(token));
```

- [ ] **Step 6: Run the whole suite**

Run: `cargo test --lib`
Expected: PASS. Fix any other references to `DnsProviderConfig.token` as a `String` (now `Option<String>`).

- [ ] **Step 7: Commit**

```bash
git add src/secrets.rs src/main.rs src/api.rs
git commit -m "feat(secrets): generalize DnsProviderConfig for hetzner/namecheap/manual"
```

---

### Task 6: Provider factory

**Files:**
- Modify: `src/dns.rs`
- Test: `src/dns.rs` `mod tests`

**Interfaces:**
- Consumes: `DnsProviderConfig` (from `crate::secrets`), all provider impls.
- Produces: `pub fn build_provider(cfg: &crate::secrets::DnsProviderConfig, client_ip: &str) -> anyhow::Result<Arc<dyn DnsProvider>>`.

`client_ip` is the box's `HOSTER_PUBLIC_IP`; only Namecheap needs it (its allowlisted `ClientIp`).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn build_provider_maps_each_kind() {
    use crate::secrets::DnsProviderConfig;
    let cf = DnsProviderConfig { kind: "cloudflare".into(), token: Some("t".into()), api_user: None, api_key: None, username: None };
    assert!(build_provider(&cf, "1.2.3.4").is_ok());
    let manual = DnsProviderConfig { kind: "manual".into(), token: None, api_user: None, api_key: None, username: None };
    assert!(build_provider(&manual, "1.2.3.4").is_ok());
    let bad = DnsProviderConfig { kind: "route53".into(), token: None, api_user: None, api_key: None, username: None };
    assert!(build_provider(&bad, "1.2.3.4").is_err());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib dns::tests::build_provider_maps_each_kind`
Expected: FAIL — `build_provider` undefined.

- [ ] **Step 3: Implement**

```rust
use std::sync::Arc;

/// Build a live provider from stored credentials. `client_ip` is hoster's
/// public IP (Namecheap's allowlisted ClientIp; ignored by the others).
pub fn build_provider(
    cfg: &crate::secrets::DnsProviderConfig,
    client_ip: &str,
) -> anyhow::Result<Arc<dyn DnsProvider>> {
    cfg.validate().map_err(|e| anyhow::anyhow!(e))?;
    let p: Arc<dyn DnsProvider> = match cfg.kind.as_str() {
        "cloudflare" => Arc::new(CloudflareProvider::new(cfg.token.clone().unwrap())),
        "hetzner" => Arc::new(HetznerProvider::new(cfg.token.clone().unwrap())),
        "namecheap" => Arc::new(NamecheapProvider::new(
            cfg.api_user.clone().unwrap(), cfg.api_key.clone().unwrap(),
            cfg.username.clone().unwrap(), client_ip.to_string())),
        "manual" => Arc::new(ManualProvider::new()),
        other => anyhow::bail!("unknown DNS provider {other:?}"),
    };
    Ok(p)
}
```

- [ ] **Step 4: Run**

Run: `cargo test --lib dns::tests::build_provider_maps_each_kind`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dns.rs
git commit -m "feat(dns): add build_provider factory over stored credentials"
```

---

### Task 7: `HOSTER_PUBLIC_IP` setting

**Files:**
- Modify: `src/settings.rs` (`Settings` struct 1-15; its `Debug` at ~38-56; test builders that construct `Settings`)
- Modify: `src/main.rs` (env read near settings construction)
- Modify: `src/engine.rs` test `Settings` builders (525-546, ~749) and any other `Settings { .. }` literal
- Test: `src/settings.rs` `mod tests`

**Interfaces:**
- Produces: `Settings.public_ip: Option<String>`.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn settings_carries_public_ip() {
    let s = Settings {
        listen: "127.0.0.1:0".into(), api_listen: "127.0.0.1:0".into(),
        hostname_template: "{service}-{branch}.dev.example.com".into(),
        registry: "".into(), token: "t".into(), dashboard_password: None,
        https_listen: None, cert_dir: "/tmp".into(), public_ip: Some("1.2.3.4".into()),
    };
    assert_eq!(s.public_ip.as_deref(), Some("1.2.3.4"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib settings::tests::settings_carries_public_ip`
Expected: FAIL — `Settings` has no field `public_ip`.

- [ ] **Step 3: Add the field**

In `src/settings.rs`, add to `Settings`:
```rust
    /// The box's public IP, published as the wildcard A record's target.
    /// Required once any non-manual DNS provider is configured.
    pub public_ip: Option<String>,
```
Its non-secret value can stay in the hand-written `Debug` (add `.field("public_ip", &self.public_ip)`).

- [ ] **Step 4: Read it from env in `main.rs`**

Where `Settings` is constructed:
```rust
        public_ip: std::env::var("HOSTER_PUBLIC_IP").ok().filter(|v| !v.trim().is_empty()),
```

- [ ] **Step 5: Update every other `Settings { .. }` literal**

Add `public_ip: None,` to the test builders in `src/settings.rs`, `src/engine.rs` (functions at 525, 539, ~749), and anywhere else the compiler flags a missing field.

- [ ] **Step 6: Run**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add src/settings.rs src/main.rs src/engine.rs
git commit -m "feat(settings): add HOSTER_PUBLIC_IP for wildcard A records"
```

---

### Task 8: Per-project DNS provider + resolver

**Files:**
- Modify: `src/secrets.rs` (`ProjectData` struct ~68-71; add store methods; add resolver)
- Test: `src/secrets.rs` `mod tests`

**Interfaces:**
- Consumes: `DnsProviderConfig`, `settings::wildcard_base`, existing `project_hostname_templates`, `hostname_template_for`, global `acme.provider`.
- Produces:
  - `Store::set_project_dns_provider(&self, project: &str, cfg: DnsProviderConfig) -> Result<(), String>`
  - `Store::project_dns_provider(&self, project: &str) -> Option<DnsProviderConfig>`
  - `Store::dns_provider_for(&self, base: &str, default_template: &str) -> Option<DnsProviderConfig>` — the project whose (own or default) template's `wildcard_base` equals `base` wins with its provider; else the global default (`acme.provider`).

- [ ] **Step 1: Write the failing resolver test**

```rust
#[test]
fn resolver_prefers_project_provider_then_global_default() {
    let dir = tempdir_path();
    let store = Store::load(&dir).unwrap();
    store.set_acme_config("me@example.com", None).unwrap();
    // global default = cloudflare
    store.set_dns_provider(DnsProviderConfig { kind: "cloudflare".into(), token: Some("cf".into()), api_user: None, api_key: None, username: None }).unwrap();
    // project "alpha" overrides with hetzner and its own template
    store.set_hostname_template("alpha", "{service}-{branch}.alpha.example.com").unwrap();
    store.set_project_dns_provider("alpha", DnsProviderConfig { kind: "hetzner".into(), token: Some("hz".into()), api_user: None, api_key: None, username: None }).unwrap();

    let default_tmpl = "{service}-{branch}.dev.example.com";
    // alpha's base resolves to hetzner
    let a = store.dns_provider_for("*.alpha.example.com", default_tmpl).unwrap();
    assert_eq!(a.kind, "hetzner");
    // an unclaimed base falls back to the global default
    let d = store.dns_provider_for("*.dev.example.com", default_tmpl).unwrap();
    assert_eq!(d.kind, "cloudflare");
}
```

(Use whatever temp-dir helper the existing `secrets` tests use in place of `tempdir_path()`; e.g. the pattern in `persists_and_reloads_from_disk`.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib secrets::tests::resolver_prefers_project_provider_then_global_default`
Expected: FAIL — methods undefined.

- [ ] **Step 3: Add the per-project field**

Extend `ProjectData`:
```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dns_provider: Option<DnsProviderConfig>,
```

- [ ] **Step 4: Add the store methods**

```rust
    pub fn set_project_dns_provider(&self, project: &str, cfg: DnsProviderConfig) -> Result<(), String> {
        cfg.validate()?;
        let mut data = self.data.lock().unwrap();
        data.projects.entry(project.to_string()).or_default().dns_provider = Some(cfg);
        self.persist(&data).map_err(|e| e.to_string())
    }
    pub fn project_dns_provider(&self, project: &str) -> Option<DnsProviderConfig> {
        self.data.lock().unwrap().projects.get(project).and_then(|p| p.dns_provider.clone())
    }
    /// Resolve the provider that owns `base` (a `*.suffix` wildcard base):
    /// the project whose template produces `base` (own provider if set), else
    /// the global default.
    pub fn dns_provider_for(&self, base: &str, default_template: &str) -> Option<DnsProviderConfig> {
        let data = self.data.lock().unwrap();
        for (name, p) in &data.projects {
            let tmpl = p.hostname_template.clone().unwrap_or_else(|| default_template.to_string());
            if crate::settings::wildcard_base(&tmpl).as_deref() == Some(base) {
                if let Some(cfg) = &p.dns_provider { return Some(cfg.clone()); }
                break;
            }
        }
        data.acme.as_ref().and_then(|a| a.provider.clone())
    }
```

- [ ] **Step 5: Run**

Run: `cargo test --lib secrets::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/secrets.rs
git commit -m "feat(secrets): per-project DNS provider with base-domain resolver"
```

---

### Task 9: Ensure the wildcard A record on deploy (idempotent, non-fatal)

**Files:**
- Modify: `src/engine.rs` (`Engine` struct 77-84; `deploy` 154+; `template_for` 366)
- Test: `src/engine.rs` `mod tests`

**Interfaces:**
- Consumes: `store.dns_provider_for`, `dns::build_provider`, `settings.public_ip`, `settings.hostname_template`, `settings::wildcard_base`, `template_for`.
- Produces: private `Engine::ensure_wildcard_dns(&self, project: &str)`; an `ensured_dns: Mutex<HashMap<String, String>>` dedup field on `Engine` (base → ip).

- [ ] **Step 1: Write the failing test**

Using the test `Engine` (built with a `Store` and `FakeRuntime`) plus a `FakeDns` is awkward because the engine builds providers from stored creds. Test the **decision + dedup** via a seam: assert that after two deploys of the same project, `ensure_wildcard_dns` recorded the base once. Add a test hook returning the dedup map.

```rust
#[tokio::test]
async fn deploy_ensures_wildcard_once_per_base() {
    let store = Arc::new(test_store());
    store.set_acme_config("me@example.com", None).unwrap();
    store.set_dns_provider(DnsProviderConfig { kind: "manual".into(), token: None, api_user: None, api_key: None, username: None }).unwrap();
    let engine = engine_with(store.clone(), tls_settings_with_ip("1.2.3.4"));
    engine.deploy(deploy_config("myproj", "feature-a")).await.unwrap();
    engine.deploy(deploy_config("myproj", "feature-b")).await.unwrap();
    let ensured = engine.ensured_dns_snapshot();
    assert_eq!(ensured.get("*.dev.example.com").map(String::as_str), Some("1.2.3.4"));
    assert_eq!(ensured.len(), 1, "one base ensured despite two deploys");
}
```

Add helpers next to the existing test builders: `tls_settings_with_ip(ip)` clones `tls_settings()` and sets `public_ip: Some(ip.into())`; `engine_with(store, settings)`; and expose:
```rust
    #[cfg(test)]
    pub fn ensured_dns_snapshot(&self) -> std::collections::HashMap<String, String> {
        self.ensured_dns.lock().unwrap().clone()
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib engine::tests::deploy_ensures_wildcard_once_per_base`
Expected: FAIL — `ensured_dns`/method missing.

- [ ] **Step 3: Add the dedup field and the ensure method**

Add to `Engine`:
```rust
    ensured_dns: Mutex<std::collections::HashMap<String, String>>,
```
Initialize it in both `Engine::new` constructors (`ensured_dns: Mutex::new(std::collections::HashMap::new())`).

```rust
    /// Ensure the project's wildcard A record points at the box's public IP.
    /// Best-effort: logs and returns Ok on any DNS error so a deploy is never
    /// blocked by DNS. Skips when the resolved provider is manual/absent, when
    /// the template has no wildcard base, or when already ensured for this
    /// (base -> ip). Re-ensures when the IP changed.
    async fn ensure_wildcard_dns(&self, project: &str) {
        let template = self.template_for(project);
        let Some(base) = crate::settings::wildcard_base(&template) else { return; };
        let cfg = self.store.dns_provider_for(&base, &self.settings.hostname_template);
        let Some(cfg) = cfg else { return; };
        if cfg.kind == "manual" { return; }
        let Some(ip) = self.settings.public_ip.clone() else {
            tracing::warn!(project, base, "DNS provider set but HOSTER_PUBLIC_IP is unset; skipping wildcard A");
            return;
        };
        if self.ensured_dns.lock().unwrap().get(&base) == Some(&ip) { return; }
        let provider = match crate::dns::build_provider(&cfg, &ip) {
            Ok(p) => p,
            Err(e) => { tracing::error!(project, base, error = %e, "failed to build DNS provider"); return; }
        };
        match provider.upsert_a(&base, &ip).await {
            Ok(()) => {
                self.ensured_dns.lock().unwrap().insert(base.clone(), ip.clone());
                tracing::info!(project, base, ip, "ensured wildcard A record");
            }
            Err(e) => tracing::error!(project, base, error = %e, "failed to ensure wildcard A record (deploy continues)"),
        }
    }
```
Note: `manual` returns before the IP check, so a manual project with no `public_ip` is fine.

- [ ] **Step 4: Call it from `deploy`**

Near the top of `deploy` (after the project/branch are known, before or alongside routing), add:
```rust
        self.ensure_wildcard_dns(&req.project).await;
```
Use the actual project accessor on `DeployRequest` (match the existing `template_for(project)` call site).

- [ ] **Step 5: Run**

Run: `cargo test --lib engine::`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/engine.rs
git commit -m "feat(engine): ensure wildcard A record on deploy (idempotent, non-fatal)"
```

---

### Task 10: Resolve the DNS provider per domain during renewal

**Files:**
- Modify: `src/main.rs` (`StoreIssuer` 37-66)
- Test: covered by existing renewal tests + a `StoreIssuer` unit check if practical; otherwise a manual verification step.

**Interfaces:**
- Consumes: `store.dns_provider_for`, `dns::build_provider`, `settings.public_ip`, `settings.hostname_template`, `acme::Issuer`.
- Produces: unchanged `CertIssuer` behavior, but the DNS provider is now resolved per requested domain.

`renewal::run_once` calls `issuer.issue(domain)` per wanted domain, where `domain` is a `*.base` wildcard. `StoreIssuer::issue` currently builds one Cloudflare provider from the global config. Replace with per-domain resolution.

- [ ] **Step 1: Rewrite `StoreIssuer::issue`**

`StoreIssuer` needs the `Store` and `Settings` (for `public_ip` and the default template). If it does not already hold them, add `store: Arc<Store>` and `settings: Arc<Settings>` fields and thread them in at construction in `main.rs`.

```rust
    async fn issue(&self, domain: &str) -> anyhow::Result<IssuedCert> {
        let cfg = self.store.acme_config().ok_or_else(|| anyhow::anyhow!("ACME is not configured"))?;
        let provider_cfg = self.store
            .dns_provider_for(domain, &self.settings.hostname_template)
            .ok_or_else(|| anyhow::anyhow!("no DNS provider configured for {domain}"))?;
        if provider_cfg.kind == "manual" {
            anyhow::bail!("{domain} uses the manual DNS provider; hoster cannot answer its DNS-01 challenge");
        }
        let client_ip = self.settings.public_ip.clone().unwrap_or_default();
        let dns = hoster::dns::build_provider(&provider_cfg, &client_ip)?;
        let issuer = Issuer::new(self.account_path.clone(), cfg.email, dns);
        issuer.issue_cert(domain).await
    }
```
Keep any existing staging/production opt-in call on `Issuer` that the current code performs (mirror the pre-change `Issuer::new(...)` usage — do not drop a `.staging(...)`/production toggle if one exists).

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles. Fix `StoreIssuer` construction in `main.rs` to pass `store` and `settings`.

- [ ] **Step 3: Run the renewal + acme suites**

Run: `cargo test --lib renewal:: acme::`
Expected: PASS (these use `FakeDns`/fake issuers and are unaffected by the resolution change).

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(acme): resolve the DNS provider per domain during issuance"
```

---

### Task 11: API — set/clear global default and per-project DNS provider

**Files:**
- Modify: `src/api.rs` (the existing DNS-token handler + its request body type; route table)
- Test: `src/api.rs` `mod tests` (follow the existing API test pattern)

**Interfaces:**
- Consumes: `store.set_dns_provider`, `store.set_project_dns_provider`, `store.delete_dns_token`, `store.masked_acme`, `store.project_dns_provider`.
- Produces: request body accepting all four kinds and each kind's fields, for both the global default and a per-project override; masked reads never echo secrets.

- [ ] **Step 1: Write the failing API test**

Mirror the existing DNS-token API test. Assert: (a) posting a hetzner global default succeeds and the masked read shows `provider_kind = "hetzner"` with no token; (b) posting a namecheap provider missing `api_key` returns a client error naming the missing field; (c) posting a per-project provider is reflected by `project_dns_provider`.

```rust
#[tokio::test]
async fn api_sets_global_and_project_dns_providers_without_leaking_secrets() {
    // ... build the test app + store as the existing DNS-token test does ...
    // 1. set ACME email first (precondition), then POST a hetzner global default
    // 2. GET the masked settings; assert body contains "hetzner" and NOT the token
    // 3. POST a namecheap provider without api_key; assert 4xx mentioning "api_key"
    // 4. POST a per-project provider for "alpha"; assert store.project_dns_provider("alpha").kind == that kind
}
```
Fill each `// ...` using the concrete helpers from the current `api::tests` DNS test (same app constructor, same bearer-token header, same JSON POST helper).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib api::tests::api_sets_global_and_project_dns_providers_without_leaking_secrets`
Expected: FAIL.

- [ ] **Step 3: Generalize the request body + handlers**

Replace the Cloudflare-only `SetDnsTokenBody` with:
```rust
#[derive(serde::Deserialize)]
struct SetDnsProviderBody {
    kind: String,
    #[serde(default)] token: Option<String>,
    #[serde(default)] api_user: Option<String>,
    #[serde(default)] api_key: Option<String>,
    #[serde(default)] username: Option<String>,
    /// When present, sets this project's override instead of the global default.
    #[serde(default)] project: Option<String>,
}
impl std::fmt::Debug for SetDnsProviderBody { /* redact token + api_key, same pattern as DnsProviderConfig */
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetDnsProviderBody")
            .field("kind", &self.kind)
            .field("token", &self.token.as_ref().map(|_| "[redacted]"))
            .field("api_user", &self.api_user)
            .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
            .field("username", &self.username)
            .field("project", &self.project)
            .finish()
    }
}
```
Handler builds a `DnsProviderConfig` from the body and dispatches:
```rust
    let cfg = hoster::secrets::DnsProviderConfig {
        kind: body.kind, token: body.token, api_user: body.api_user,
        api_key: body.api_key, username: body.username,
    };
    let result = match body.project {
        Some(project) => store.set_project_dns_provider(&project, cfg),
        None => store.set_dns_provider(cfg),
    };
    result.map_err(/* existing 400/validation mapping */)?;
```
Keep the existing masked-settings GET; it already omits secrets via `MaskedAcme`.

- [ ] **Step 4: Run**

Run: `cargo test --lib api::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/api.rs
git commit -m "feat(api): set global + per-project DNS provider across all kinds"
```

---

### Task 12: Guided setup UI

**Files:**
- Modify: `src/ui/settings.rs` (the DNS panel; test at 260 `renders_the_dns_provider_masked_and_with_manage_actions`)
- Test: `src/ui/settings.rs` `mod tests`

**Interfaces:**
- Consumes: masked settings (`provider_kind`, `token_set`), `settings.public_ip`, the set of project base domains (`wildcard_base` of each template).
- Produces: DNS panel markup with a provider picker, per-kind instructions, manual-mode record display, public-IP state, and a verify affordance.

- [ ] **Step 1: Write the failing render tests**

```rust
#[test]
fn dns_panel_lists_all_four_providers_and_shows_public_ip() {
    let html = render_dns_panel(/* masked acme with provider_kind = None */, Some("1.2.3.4"), &["*.dev.example.com".to_string()]);
    for kind in ["cloudflare", "hetzner", "namecheap", "manual"] {
        assert!(html.contains(kind), "picker must offer {kind}");
    }
    assert!(html.contains("1.2.3.4"), "must surface HOSTER_PUBLIC_IP");
}

#[test]
fn dns_panel_manual_mode_shows_the_record_to_create() {
    // manual selected + base present -> the literal record is displayed for copy-paste
    let html = render_dns_panel_manual(&["*.dev.example.com".to_string()], Some("1.2.3.4"));
    assert!(html.contains("*.dev.example.com"));
    assert!(html.contains("A"));
    assert!(html.contains("1.2.3.4"));
}

#[test]
fn dns_panel_flags_missing_public_ip_for_non_manual() {
    let html = render_dns_panel(/* provider_kind = Some("cloudflare") */, None, &["*.dev.example.com".to_string()]);
    assert!(html.to_lowercase().contains("hoster_public_ip"), "must warn the IP is unset");
}

#[test]
fn dns_panel_shows_namecheap_allowlist_precondition() {
    let html = render_namecheap_help("1.2.3.4");
    assert!(html.to_lowercase().contains("allowlist") || html.to_lowercase().contains("whitelist"));
    assert!(html.contains("1.2.3.4"), "show the IP the operator must allowlist");
}
```

Adjust the exact helper names/signatures to the module's existing render style (the current test at line 260 shows how the panel is rendered and asserted — follow it; extract small `render_*` helpers if the panel is currently one function).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib ui::settings::tests::dns_panel_lists_all_four_providers_and_shows_public_ip`
Expected: FAIL.

- [ ] **Step 3: Implement the guided panel**

In `src/ui/settings.rs`, extend the DNS panel to render:
- A provider `<select>` with `cloudflare | hetzner | namecheap | manual`, revealing only the selected kind's fields.
- Per-kind help text: Cloudflare (scoped Zone.DNS token), Hetzner (DNS console token), Namecheap (three fields **and** the allowlist precondition showing `public_ip`), Manual (no fields).
- The resolved `HOSTER_PUBLIC_IP` value, and when it is `None` while a non-manual provider is set, an inline warning naming `HOSTER_PUBLIC_IP`.
- For manual mode (or always, as a "records hoster will manage" summary), the literal `*.<base>  A  <public_ip>` line for each project base domain, plus the `_acme-challenge.<base>  TXT` note when TLS is on.
- A "check" button/affordance next to a saved non-manual provider (posts to a verify endpoint or links to it) — rendering only; wiring the resolver check is optional and may be a follow-up if the verify endpoint doesn't exist yet. If you add the affordance without a backend, label it clearly and keep the test asserting only its presence.

Keep secrets write-only: never render `token`/`api_key` values; show `token_set`-style booleans exactly as the current panel does.

- [ ] **Step 4: Run**

Run: `cargo test --lib ui::`
Expected: PASS.

- [ ] **Step 5: Full build + test + doc note**

Run: `cargo test` and `cargo build`
Expected: PASS.
Update `docs/deploying.md`: add a short "DNS providers" subsection linking the four kinds, `HOSTER_PUBLIC_IP`, and the Namecheap allowlist caveat.

- [ ] **Step 6: Commit**

```bash
git add src/ui/settings.rs docs/deploying.md
git commit -m "feat(ui): guided DNS provider setup with manual-mode records and IP checks"
```

---

## Self-Review

**Spec coverage:**
- Four providers (CF/Hetzner/Namecheap/Manual) → Tasks 1–4. ✓
- A-record trait extension (replace semantics) → Task 1. ✓
- Credential model generalization + redaction + back-compat → Task 5. ✓
- Provider factory → Task 6. ✓
- `HOSTER_PUBLIC_IP` + required-when-non-manual validation → Task 7 (field/env) + enforced at use in Tasks 9/10 (skip + warn) and surfaced in UI Task 12. ✓
- Per-project provider + global default + resolver → Task 8. ✓
- Wildcard A on deploy, idempotent, non-fatal, not deleted on teardown → Task 9. ✓
- ACME per-domain provider resolution → Task 10. ✓
- API for all kinds, global + per-project, no secret leaks → Task 11. ✓
- Guided UI (per-kind help, manual records, allowlist, IP state, verify) → Task 12. ✓
- Testing strategy (mock_server per provider, Namecheap sibling preservation, resolution fallback, redaction) → Tasks 1–3, 5, 8. ✓

**Note on a spec deviation:** the spec sketched credentials as an `enum DnsCredentials`; the plan realizes them as flat `Option<String>` fields on `DnsProviderConfig`. This is deliberate — it makes the legacy `{kind, token}` on-disk format deserialize with zero migration code, satisfying the backward-compatibility constraint. Same behavior, simpler and safer.

**Placeholder scan:** No `TBD`/`TODO`. Two tasks (11, 12) intentionally say "follow the existing test/render pattern in this file" and point at the concrete anchor (the current DNS API test; `ui/settings.rs:260`) rather than duplicating that harness blind — the implementer reads the neighboring code that is guaranteed to exist. All new logic ships with concrete code.

**Type consistency:** `DnsProviderConfig` fields (`kind`, `token`, `api_user`, `api_key`, `username`) are identical across Tasks 5, 6, 8, 11. `build_provider(&cfg, client_ip)` signature matches its callers in Tasks 9 and 10. `dns_provider_for(base, default_template)` matches callers in Tasks 9 and 10. `Settings.public_ip: Option<String>` matches every consumer.
