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
}

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
        self.records
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_default()
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
}
