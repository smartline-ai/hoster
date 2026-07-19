//! Hoster-managed project environment variables.
//!
//! A per-project store of env vars (secrets like API keys) that the engine
//! injects into a project's services at deploy time — without baking them into
//! the image or the repo's `hoster.json`. Persisted as a `0600` JSON file,
//! written atomically. Values are never handed back out through the masked
//! read path used by the dashboard and API.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::is_dns_label;
use crate::settings::validate_hostname_template;

/// Largest accepted value, guarding against a runaway paste filling the store.
pub const MAX_VALUE_LEN: usize = 32 * 1024;

/// One stored variable: its secret value and the services it targets. An empty
/// `services` list means every service in the project.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Var {
    pub value: String,
    #[serde(default)]
    pub services: Vec<String>,
}

/// A hand-written `Debug` — the derived one would print `value` (an arbitrary
/// secret: an API key, a DB password, anything a project stores) in full.
/// `Debug` is not protected by a masked type the way serialization is, so
/// this has to redact explicitly rather than by construction.
impl std::fmt::Debug for Var {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Var")
            .field("value", &"[redacted]")
            .field("services", &self.services)
            .finish()
    }
}

/// A project's container-registry credential. One per project; applied at pull
/// time only to images whose host matches `registry`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryCred {
    pub registry: String,
    pub username: String,
    pub password: String,
}

/// A hand-written `Debug` that redacts `password` — see [`Var`]'s impl for
/// why this cannot be `derive`d.
impl std::fmt::Debug for RegistryCred {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegistryCred")
            .field("registry", &self.registry)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectData {
    #[serde(default)]
    vars: BTreeMap<String, Var>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registry: Option<RegistryCred>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname_template: Option<String>,
}

/// A DNS provider's credentials. Every secret field here can rewrite DNS —
/// treat all of them as the most dangerous secrets in the store. This leaves
/// this module only through [`Store::acme_config`], which exists solely to
/// feed issuance.
///
/// Different providers need different fields: Cloudflare and Hetzner use a
/// single API `token`; Namecheap needs `api_user` + `api_key` + `username`.
/// Rather than an enum (which would break the on-disk format for existing
/// `{"kind":"cloudflare","token":"..."}` configs), every field stays flat and
/// optional — `kind` says which ones matter, and [`DnsProviderConfig::validate`]
/// enforces that per kind.
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

/// A hand-written `Debug` that redacts `token` and `api_key` while still
/// showing `kind`, `api_user`, and `username` — the masked [`MaskedAcme`] type
/// protects serialization, but a derived `Debug` here would print the secrets
/// in full the moment anything (now or in future code) formats this with
/// `{:?}`. `Debug` gets no such protection from a separate type, so it has to
/// be redacted by hand.
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
    /// Checks that the fields the provider `kind` needs are present and
    /// non-blank. Called before a config is ever stored, so a partially
    /// filled-in credential never sits on disk waiting to fail at the next
    /// renewal pass instead of at submission time.
    pub fn validate(&self) -> Result<(), String> {
        let need = |o: &Option<String>, f: &str| -> Result<(), String> {
            match o {
                Some(v) if v.trim().is_empty() => Err(format!("{} requires {f}", self.kind)),
                Some(v) if v.len() > MAX_VALUE_LEN => {
                    Err(format!("{f} too long (max {MAX_VALUE_LEN} bytes)"))
                }
                Some(_) => Ok(()),
                None => Err(format!("{} requires {f}", self.kind)),
            }
        };
        match self.kind.as_str() {
            "cloudflare" | "hetzner" => need(&self.token, "an API token"),
            "namecheap" => {
                need(&self.api_user, "api_user")?;
                need(&self.api_key, "api_key")?;
                need(&self.username, "username")
            }
            "manual" => Ok(()),
            other => Err(format!(
                "unknown DNS provider {other:?}; supported: cloudflare, hetzner, namecheap, manual"
            )),
        }
    }
}

/// The ACME account settings, plus (optionally) the DNS credentials issuance
/// needs. Global, not per-project: one account issues for every domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcmeConfig {
    pub email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<DnsProviderConfig>,
}

/// ACME configuration as exposed to the UI/API: **never** the token.
///
/// This is a separate type rather than a serde attribute on [`AcmeConfig`] so
/// that leaking the token is not a matter of remembering to skip a field —
/// there is simply no field here that could carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedAcme {
    pub email: String,
    pub control_hostname: Option<String>,
    pub provider_kind: Option<String>,
    pub token_set: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Data {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, ProjectData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acme: Option<AcmeConfig>,
}

impl Default for Data {
    fn default() -> Self {
        Data {
            version: 1,
            projects: BTreeMap::new(),
            acme: None,
        }
    }
}

/// A variable as exposed to the UI/API: key and target services, **never** the
/// value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedVar {
    pub key: String,
    pub services: Vec<String>,
}

/// A registry credential as exposed to the UI/API: host and username,
/// **never** the password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedRegistry {
    pub registry: String,
    pub username: String,
}

/// A project's masked variables, for listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedProject {
    pub project: String,
    pub vars: Vec<MaskedVar>,
    pub registry: Option<MaskedRegistry>,
    pub hostname_template: Option<String>,
}

/// Thread-safe, file-backed store. Persists on every mutation.
pub struct Store {
    path: PathBuf,
    data: Mutex<Data>,
}

impl Store {
    /// Load the store from `path`, or start empty if the file is absent.
    pub fn load(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let data = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)
                .map_err(|e| anyhow::anyhow!("invalid {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        };
        Ok(Store {
            path,
            data: Mutex::new(data),
        })
    }

    /// Set (upsert) a variable for a project. Validates key, services, and
    /// value size; returns a human message on the first violation.
    pub fn set_var(
        &self,
        project: &str,
        key: &str,
        value: &str,
        services: Vec<String>,
    ) -> Result<(), String> {
        if !is_project_name(project) {
            return Err(format!(
                "project name {project:?} must be non-empty and use only letters, digits, '.', '-', '_'"
            ));
        }
        if !is_env_key(key) {
            return Err(format!("env key {key:?} must match [A-Za-z_][A-Za-z0-9_]*"));
        }
        if value.len() > MAX_VALUE_LEN {
            return Err(format!("value too long (max {MAX_VALUE_LEN} bytes)"));
        }
        for svc in &services {
            if !is_dns_label(svc) {
                return Err(format!(
                    "target service {svc:?} must be a DNS label (lowercase letters, digits, hyphens)"
                ));
            }
        }
        let mut data = self.data.lock().unwrap();
        data.projects
            .entry(project.to_string())
            .or_default()
            .vars
            .insert(
                key.to_string(),
                Var {
                    value: value.to_string(),
                    services,
                },
            );
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Remove a single variable. No error if it wasn't there.
    pub fn delete_var(&self, project: &str, key: &str) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(p) = data.projects.get_mut(project) {
            p.vars.remove(key);
            if p.vars.is_empty() && p.registry.is_none() && p.hostname_template.is_none() {
                data.projects.remove(project);
            }
        }
        self.persist(&data)
    }

    /// Remove all stored variables for a project.
    pub fn delete_project(&self, project: &str) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        data.projects.remove(project);
        self.persist(&data)
    }

    /// Set (replace) the project's registry credential. Validates the project
    /// name, requires non-empty host and username, and caps the password at
    /// `MAX_VALUE_LEN`. The credential is not verified against the registry —
    /// a bad one surfaces as a failed pull at deploy time.
    pub fn set_registry(
        &self,
        project: &str,
        registry: &str,
        username: &str,
        password: &str,
    ) -> Result<(), String> {
        if !is_project_name(project) {
            return Err(format!(
                "project name {project:?} must be non-empty and use only letters, digits, '.', '-', '_'"
            ));
        }
        if registry.trim().is_empty() {
            return Err("registry host must not be empty".to_string());
        }
        if username.trim().is_empty() {
            return Err("registry username must not be empty".to_string());
        }
        if password.len() > MAX_VALUE_LEN {
            return Err(format!("password too long (max {MAX_VALUE_LEN} bytes)"));
        }
        let mut data = self.data.lock().unwrap();
        data.projects
            .entry(project.to_string())
            .or_default()
            .registry = Some(RegistryCred {
            registry: registry.trim().to_ascii_lowercase(),
            username: username.to_string(),
            password: password.to_string(),
        });
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Remove the project's registry credential. No error if there wasn't one.
    pub fn delete_registry(&self, project: &str) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(p) = data.projects.get_mut(project) {
            p.registry = None;
            if p.vars.is_empty() && p.hostname_template.is_none() {
                data.projects.remove(project);
            }
        }
        self.persist(&data)
    }

    /// The project's registry credential, if it has one.
    pub fn registry_for(&self, project: &str) -> Option<RegistryCred> {
        let data = self.data.lock().unwrap();
        data.projects.get(project).and_then(|p| p.registry.clone())
    }

    /// Set (replace) the project's hostname template. Validates the project
    /// name and the template; nothing is stored when either is rejected.
    pub fn set_hostname_template(&self, project: &str, template: &str) -> Result<(), String> {
        if !is_project_name(project) {
            return Err(format!(
                "project name {project:?} must be non-empty and use only letters, digits, '.', '-', '_'"
            ));
        }
        validate_hostname_template(template)?;
        let mut data = self.data.lock().unwrap();
        data.projects
            .entry(project.to_string())
            .or_default()
            .hostname_template = Some(template.to_string());
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Remove the project's hostname template, reverting it to the global
    /// default. No error if it did not have one.
    pub fn delete_hostname_template(&self, project: &str) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(p) = data.projects.get_mut(project) {
            p.hostname_template = None;
            if p.vars.is_empty() && p.registry.is_none() {
                data.projects.remove(project);
            }
        }
        self.persist(&data)
    }

    /// The project's hostname template, if it has one.
    pub fn hostname_template_for(&self, project: &str) -> Option<String> {
        let data = self.data.lock().unwrap();
        data.projects
            .get(project)
            .and_then(|p| p.hostname_template.clone())
    }

    /// The variables to inject into `service` of `project`: every var whose
    /// target list is empty (all services) or contains `service`.
    pub fn env_for(&self, project: &str, service: &str) -> BTreeMap<String, String> {
        let data = self.data.lock().unwrap();
        let Some(p) = data.projects.get(project) else {
            return BTreeMap::new();
        };
        p.vars
            .iter()
            .filter(|(_, v)| v.services.is_empty() || v.services.iter().any(|s| s == service))
            .map(|(k, v)| (k.clone(), v.value.clone()))
            .collect()
    }

    /// Masked listing of every project's variables. Never includes values.
    pub fn list_masked(&self) -> Vec<MaskedProject> {
        let data = self.data.lock().unwrap();
        data.projects
            .iter()
            .map(|(project, p)| MaskedProject {
                project: project.clone(),
                vars: p
                    .vars
                    .iter()
                    .map(|(key, v)| MaskedVar {
                        key: key.clone(),
                        services: v.services.clone(),
                    })
                    .collect(),
                registry: p.registry.as_ref().map(|c| MaskedRegistry {
                    registry: c.registry.clone(),
                    username: c.username.clone(),
                }),
                hostname_template: p.hostname_template.clone(),
            })
            .collect()
    }

    /// Set the ACME account email and optional control hostname, keeping any
    /// stored DNS credentials — changing the email must not silently discard
    /// the token, or the next renewal pass would fail with no visible cause.
    pub fn set_acme_config(
        &self,
        email: &str,
        control_hostname: Option<&str>,
    ) -> Result<(), String> {
        if !email.contains('@') || email.len() < 3 {
            return Err(format!("{email:?} is not a valid email address"));
        }
        if let Some(h) = control_hostname {
            crate::settings::validate_hostname(h)?;
        }
        let mut data = self.data.lock().unwrap();
        match data.acme.as_mut() {
            Some(a) => {
                a.email = email.to_string();
                a.control_hostname = control_hostname.map(str::to_string);
            }
            None => {
                data.acme = Some(AcmeConfig {
                    email: email.to_string(),
                    control_hostname: control_hostname.map(str::to_string),
                    provider: None,
                })
            }
        }
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Set the global default DNS provider credentials. Requires the ACME
    /// email to be set first, since issuance needs both. Validates the
    /// config against its `kind` before storing anything.
    pub fn set_dns_provider(&self, cfg: DnsProviderConfig) -> Result<(), String> {
        cfg.validate()?;
        let mut data = self.data.lock().unwrap();
        let acme = data
            .acme
            .as_mut()
            .ok_or("set the ACME email before a DNS provider")?;
        acme.provider = Some(cfg);
        self.persist(&data).map_err(|e| e.to_string())
    }

    /// Remove the DNS credentials, keeping the rest of the ACME config.
    pub fn delete_dns_token(&self) -> anyhow::Result<()> {
        let mut data = self.data.lock().unwrap();
        if let Some(a) = data.acme.as_mut() {
            a.provider = None;
        }
        self.persist(&data)
    }

    /// Full ACME config including the token — for issuance only, never a read
    /// path. Everything user-facing must go through [`Store::masked_acme`].
    pub fn acme_config(&self) -> Option<AcmeConfig> {
        self.data.lock().unwrap().acme.clone()
    }

    /// ACME config for display. Structurally cannot carry the token.
    pub fn masked_acme(&self) -> Option<MaskedAcme> {
        self.data.lock().unwrap().acme.as_ref().map(|a| MaskedAcme {
            email: a.email.clone(),
            control_hostname: a.control_hostname.clone(),
            provider_kind: a.provider.as_ref().map(|p| p.kind.clone()),
            token_set: a.provider.is_some(),
        })
    }

    /// Every distinct hostname template configured on a project. Used to work
    /// out which wildcard certificates are needed.
    pub fn project_hostname_templates(&self) -> Vec<String> {
        let data = self.data.lock().unwrap();
        let mut out: Vec<String> = data
            .projects
            .values()
            .filter_map(|p| p.hostname_template.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Serialize and write atomically with owner-only permissions.
    fn persist(&self, data: &Data) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(data)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// A valid POSIX-ish env var name: leading letter/underscore, then
/// letters/digits/underscores.
fn is_env_key(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A project identifier safe to embed in a URL path segment.
fn is_project_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A minimal Cloudflare-shaped `DnsProviderConfig` for tests that only
    /// care about the token — most of the DNS-provider test suite predates
    /// the `api_user`/`api_key`/`username` fields.
    fn cf(token: &str) -> DnsProviderConfig {
        DnsProviderConfig {
            kind: "cloudflare".to_string(),
            token: Some(token.to_string()),
            api_user: None,
            api_key: None,
            username: None,
        }
    }

    /// A unique, non-existent path under the OS temp dir for one test.
    fn temp_file() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "hoster-secrets-test-{}-{n}/projects.json",
            std::process::id()
        ))
    }

    #[test]
    fn set_then_env_for_returns_the_value() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("odinvestor", "GOOGLE_API_KEY", "AIza123", vec![])
            .unwrap();
        let env = s.env_for("odinvestor", "backend");
        assert_eq!(
            env.get("GOOGLE_API_KEY").map(String::as_str),
            Some("AIza123")
        );
    }

    #[test]
    fn empty_target_applies_to_every_service() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "SHARED", "v", vec![]).unwrap();
        assert!(s.env_for("p", "backend").contains_key("SHARED"));
        assert!(s.env_for("p", "postgres").contains_key("SHARED"));
    }

    #[test]
    fn specific_target_reaches_only_listed_services() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "GOOGLE_API_KEY", "k", vec!["backend".into()])
            .unwrap();
        assert!(s.env_for("p", "backend").contains_key("GOOGLE_API_KEY"));
        assert!(!s.env_for("p", "postgres").contains_key("GOOGLE_API_KEY"));
    }

    #[test]
    fn env_for_unknown_project_is_empty() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.env_for("nope", "backend").is_empty());
    }

    #[test]
    fn delete_var_removes_it() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.delete_var("p", "K").unwrap();
        assert!(!s.env_for("p", "backend").contains_key("K"));
    }

    #[test]
    fn delete_project_removes_all_its_vars() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "A", "1", vec![]).unwrap();
        s.set_var("p", "B", "2", vec![]).unwrap();
        s.delete_project("p").unwrap();
        assert!(s.env_for("p", "backend").is_empty());
    }

    #[test]
    fn persists_and_reloads_from_disk() {
        let path = temp_file();
        {
            let s = Store::load(&path).unwrap();
            s.set_var("p", "K", "secret", vec!["backend".into()])
                .unwrap();
        }
        let s2 = Store::load(&path).unwrap();
        assert_eq!(
            s2.env_for("p", "backend").get("K").map(String::as_str),
            Some("secret")
        );
    }

    #[test]
    fn stored_file_is_owner_only_and_valid_json() {
        let path = temp_file();
        let s = Store::load(&path).unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        serde_json::from_str::<serde_json::Value>(&raw).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
        }
    }

    #[test]
    fn masked_listing_never_exposes_values() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "GOOGLE_API_KEY", "topsecret", vec!["backend".into()])
            .unwrap();
        let masked = s.list_masked();
        let json = serde_json::to_string(&masked).unwrap();
        assert!(!json.contains("topsecret"), "value leaked: {json}");
        assert!(json.contains("GOOGLE_API_KEY"));
        assert!(json.contains("backend"));
    }

    #[test]
    fn rejects_invalid_env_key() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_var("p", "1BAD", "v", vec![]).is_err());
        assert!(s.set_var("p", "has space", "v", vec![]).is_err());
        assert!(s.set_var("p", "", "v", vec![]).is_err());
    }

    #[test]
    fn rejects_invalid_target_service_name() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_var("p", "K", "v", vec!["Bad_Upper".into()]).is_err());
    }

    #[test]
    fn rejects_oversized_value() {
        let s = Store::load(temp_file()).unwrap();
        let big = "x".repeat(MAX_VALUE_LEN + 1);
        assert!(s.set_var("p", "K", &big, vec![]).is_err());
    }

    #[test]
    fn rejects_invalid_project_name() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_var("bad/project", "K", "v", vec![]).is_err());
    }

    #[test]
    fn set_then_registry_for_returns_the_credential() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "ghp_secret").unwrap();
        let c = s.registry_for("p").unwrap();
        assert_eq!(c.registry, "ghcr.io");
        assert_eq!(c.username, "bot");
        assert_eq!(c.password, "ghp_secret");
    }

    #[test]
    fn registry_for_unknown_project_is_none() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.registry_for("nope").is_none());
    }

    #[test]
    fn set_registry_replaces_the_previous_one() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "old", "a").unwrap();
        s.set_registry("p", "ghcr.io", "new", "b").unwrap();
        let c = s.registry_for("p").unwrap();
        assert_eq!(c.username, "new");
        assert_eq!(c.password, "b");
    }

    #[test]
    fn delete_registry_removes_it_and_is_idempotent() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_registry("p").unwrap();
        assert!(s.registry_for("p").is_none());
        s.delete_registry("p").unwrap(); // no error the second time
    }

    #[test]
    fn masked_listing_never_exposes_the_registry_password() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "ghp_topsecret")
            .unwrap();
        let masked = s.list_masked();
        let json = serde_json::to_string(&masked).unwrap();
        assert!(!json.contains("ghp_topsecret"), "password leaked: {json}");
        assert!(json.contains("ghcr.io"));
        assert!(json.contains("bot"));
    }

    #[test]
    fn project_with_only_a_credential_is_listed() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        let masked = s.list_masked();
        assert_eq!(masked.len(), 1);
        assert_eq!(masked[0].project, "p");
        assert!(masked[0].vars.is_empty());
        assert_eq!(masked[0].registry.as_ref().unwrap().username, "bot");
    }

    #[test]
    fn deleting_the_last_var_keeps_the_credential() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.delete_var("p", "K").unwrap();
        assert!(
            s.registry_for("p").is_some(),
            "credential was pruned along with the last var"
        );
    }

    #[test]
    fn delete_project_removes_the_credential_too() {
        let s = Store::load(temp_file()).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_project("p").unwrap();
        assert!(s.registry_for("p").is_none());
    }

    #[test]
    fn credential_persists_and_reloads_from_disk() {
        let path = temp_file();
        {
            let s = Store::load(&path).unwrap();
            s.set_registry("p", "ghcr.io", "bot", "ghp_secret").unwrap();
        }
        let s2 = Store::load(&path).unwrap();
        assert_eq!(s2.registry_for("p").unwrap().password, "ghp_secret");
    }

    #[test]
    fn a_file_without_a_credential_still_loads() {
        let path = temp_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"projects":{"p":{"vars":{"K":{"value":"v","services":[]}}}}}"#,
        )
        .unwrap();
        let s = Store::load(&path).unwrap();
        assert_eq!(
            s.env_for("p", "backend").get("K").map(String::as_str),
            Some("v")
        );
        assert!(s.registry_for("p").is_none());
    }

    #[test]
    fn rejects_empty_registry_or_username() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_registry("p", "", "bot", "x").is_err());
        assert!(s.set_registry("p", "ghcr.io", "", "x").is_err());
    }

    #[test]
    fn rejects_oversized_registry_password() {
        let s = Store::load(temp_file()).unwrap();
        let big = "x".repeat(MAX_VALUE_LEN + 1);
        assert!(s.set_registry("p", "ghcr.io", "bot", &big).is_err());
    }

    #[test]
    fn rejects_invalid_project_name_for_registry() {
        let s = Store::load(temp_file()).unwrap();
        assert!(
            s.set_registry("bad/project", "ghcr.io", "bot", "x")
                .is_err()
        );
    }

    #[test]
    fn deleting_registry_keeps_the_var() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_registry("p").unwrap();
        assert!(s.registry_for("p").is_none());
        assert_eq!(
            s.env_for("p", "backend").get("K").map(String::as_str),
            Some("v"),
            "var was pruned along with the credential"
        );
    }

    #[test]
    fn deleting_var_then_registry_fully_cleans_up_a_mixed_project() {
        let s = Store::load(temp_file()).unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_var("p", "K").unwrap();
        assert!(s.env_for("p", "backend").is_empty());
        assert!(
            s.registry_for("p").is_some(),
            "credential was pruned along with the only var"
        );
        s.delete_registry("p").unwrap();
        assert!(s.registry_for("p").is_none());
        assert!(s.env_for("p", "backend").is_empty());
    }

    #[test]
    fn set_then_hostname_template_for_returns_it() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();
        assert_eq!(
            s.hostname_template_for("p").as_deref(),
            Some("{branch}.demo.example.com")
        );
    }

    #[test]
    fn hostname_template_for_unknown_project_is_none() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.hostname_template_for("nope").is_none());
    }

    #[test]
    fn set_hostname_template_replaces_the_previous_one() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.a.example.com")
            .unwrap();
        s.set_hostname_template("p", "{branch}.b.example.com")
            .unwrap();
        assert_eq!(
            s.hostname_template_for("p").as_deref(),
            Some("{branch}.b.example.com")
        );
    }

    #[test]
    fn delete_hostname_template_removes_it_and_is_idempotent() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();
        s.delete_hostname_template("p").unwrap();
        assert!(s.hostname_template_for("p").is_none());
        s.delete_hostname_template("p").unwrap();
    }

    #[test]
    fn set_hostname_template_rejects_an_invalid_template() {
        let s = Store::load(temp_file()).unwrap();
        assert!(
            s.set_hostname_template("p", "{service}.example.com")
                .is_err()
        );
        assert!(s.set_hostname_template("p", "").is_err());
        assert!(
            s.hostname_template_for("p").is_none(),
            "nothing should be stored on rejection"
        );
    }

    #[test]
    fn set_hostname_template_rejects_an_invalid_project_name() {
        let s = Store::load(temp_file()).unwrap();
        assert!(
            s.set_hostname_template("bad/project", "{branch}.example.com")
                .is_err()
        );
    }

    #[test]
    fn project_with_only_a_hostname_template_is_listed() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();
        let masked = s.list_masked();
        assert_eq!(masked.len(), 1);
        assert_eq!(masked[0].project, "p");
        assert_eq!(
            masked[0].hostname_template.as_deref(),
            Some("{branch}.demo.example.com")
        );
    }

    #[test]
    fn deleting_the_last_var_keeps_the_hostname_template() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.delete_var("p", "K").unwrap();
        assert!(
            s.hostname_template_for("p").is_some(),
            "template pruned with the last var"
        );
    }

    #[test]
    fn deleting_the_registry_keeps_the_hostname_template() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_registry("p").unwrap();
        assert!(
            s.hostname_template_for("p").is_some(),
            "template pruned with the registry"
        );
    }

    #[test]
    fn deleting_the_hostname_template_keeps_vars_and_registry() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_hostname_template("p").unwrap();
        assert_eq!(
            s.env_for("p", "backend").get("K").map(String::as_str),
            Some("v")
        );
        assert!(s.registry_for("p").is_some());
    }

    #[test]
    fn hostname_template_persists_and_reloads_from_disk() {
        let path = temp_file();
        {
            let s = Store::load(&path).unwrap();
            s.set_hostname_template("p", "{branch}.demo.example.com")
                .unwrap();
        }
        let s2 = Store::load(&path).unwrap();
        assert_eq!(
            s2.hostname_template_for("p").as_deref(),
            Some("{branch}.demo.example.com")
        );
    }

    #[test]
    fn masked_acme_never_exposes_the_dns_token() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", Some("hoster.example.com"))
            .unwrap();
        s.set_dns_provider(cf("cf_topsecret_token")).unwrap();
        let masked = s.masked_acme().unwrap();
        let json = serde_json::to_string(&masked).unwrap();
        assert!(!json.contains("cf_topsecret_token"), "token leaked: {json}");
        assert!(json.contains("me@example.com"));
        assert!(masked.token_set);
    }

    #[test]
    fn acme_config_round_trips_including_the_token() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", None).unwrap();
        s.set_dns_provider(cf("tok")).unwrap();
        let cfg = s.acme_config().unwrap();
        assert_eq!(cfg.email, "me@example.com");
        assert_eq!(cfg.provider.unwrap().token.as_deref(), Some("tok"));
    }

    #[test]
    fn delete_dns_token_keeps_the_email() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", None).unwrap();
        s.set_dns_provider(cf("tok")).unwrap();
        s.delete_dns_token().unwrap();
        let masked = s.masked_acme().unwrap();
        assert_eq!(masked.email, "me@example.com");
        assert!(!masked.token_set);
    }

    #[test]
    fn set_acme_config_rejects_an_email_without_an_at_sign() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_acme_config("not-an-email", None).is_err());
    }

    #[test]
    fn set_dns_provider_rejects_an_unknown_provider_kind() {
        let s = Store::load(temp_file()).unwrap();
        let bad = DnsProviderConfig {
            kind: "bind9".to_string(),
            token: Some("tok".to_string()),
            api_user: None,
            api_key: None,
            username: None,
        };
        assert!(s.set_dns_provider(bad).is_err());
    }

    #[test]
    fn set_dns_provider_requires_the_email_first() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_dns_provider(cf("tok")).is_err());
    }

    #[test]
    fn set_acme_config_keeps_an_existing_dns_token() {
        let s = Store::load(temp_file()).unwrap();
        s.set_acme_config("me@example.com", None).unwrap();
        s.set_dns_provider(cf("tok")).unwrap();
        s.set_acme_config("other@example.com", Some("hoster.example.com"))
            .unwrap();
        let cfg = s.acme_config().unwrap();
        assert_eq!(cfg.email, "other@example.com");
        assert_eq!(
            cfg.provider.and_then(|p| p.token),
            Some("tok".to_string()),
            "changing the email must not discard the DNS credentials"
        );
    }

    #[test]
    fn set_acme_config_rejects_an_invalid_control_hostname() {
        let s = Store::load(temp_file()).unwrap();
        assert!(
            s.set_acme_config("me@example.com", Some("Not A Hostname"))
                .is_err()
        );
        assert!(
            s.masked_acme().is_none(),
            "nothing should be stored on rejection"
        );
    }

    #[test]
    fn acme_config_persists_and_reloads_from_disk() {
        let path = temp_file();
        {
            let s = Store::load(&path).unwrap();
            s.set_acme_config("me@example.com", Some("hoster.example.com"))
                .unwrap();
            s.set_dns_provider(cf("tok")).unwrap();
        }
        let s2 = Store::load(&path).unwrap();
        let cfg = s2.acme_config().unwrap();
        assert_eq!(cfg.control_hostname.as_deref(), Some("hoster.example.com"));
        assert_eq!(cfg.provider.unwrap().token.as_deref(), Some("tok"));
    }

    #[test]
    fn a_file_without_acme_config_still_loads() {
        let path = temp_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"projects":{"p":{"vars":{"K":{"value":"v","services":[]}}}}}"#,
        )
        .unwrap();
        let s = Store::load(&path).unwrap();
        assert!(s.acme_config().is_none());
        assert!(s.masked_acme().is_none());
    }

    #[test]
    fn a_file_without_a_hostname_template_still_loads() {
        let path = temp_file();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"projects":{"p":{"vars":{"K":{"value":"v","services":[]}}}}}"#,
        )
        .unwrap();
        let s = Store::load(&path).unwrap();
        assert_eq!(
            s.env_for("p", "backend").get("K").map(String::as_str),
            Some("v")
        );
        assert!(s.hostname_template_for("p").is_none());
    }

    #[test]
    fn dns_provider_config_debug_redacts_the_token() {
        let cfg = cf("cf_topsecret_token");
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("cf_topsecret_token"),
            "token leaked via Debug: {dbg}"
        );
        assert!(dbg.contains("cloudflare"), "kind should still be visible");
    }

    #[test]
    fn registry_cred_debug_redacts_the_password() {
        let cred = RegistryCred {
            registry: "ghcr.io".to_string(),
            username: "bot".to_string(),
            password: "ghp_topsecret".to_string(),
        };
        let dbg = format!("{cred:?}");
        assert!(
            !dbg.contains("ghp_topsecret"),
            "password leaked via Debug: {dbg}"
        );
        assert!(dbg.contains("ghcr.io"), "registry should still be visible");
        assert!(dbg.contains("bot"), "username should still be visible");
    }

    #[test]
    fn var_debug_redacts_the_value() {
        let v = Var {
            value: "topsecret".to_string(),
            services: vec!["backend".to_string()],
        };
        let dbg = format!("{v:?}");
        assert!(!dbg.contains("topsecret"), "value leaked via Debug: {dbg}");
        assert!(dbg.contains("backend"), "services should still be visible");
    }

    #[test]
    fn dns_config_debug_redacts_all_secrets() {
        let cfg = DnsProviderConfig {
            kind: "namecheap".into(),
            token: None,
            api_user: Some("u".into()),
            api_key: Some("SECRETKEY".into()),
            username: Some("u".into()),
        };
        let shown = format!("{cfg:?}");
        assert!(!shown.contains("SECRETKEY"), "api_key leaked: {shown}");
        assert!(shown.contains("namecheap"));
    }

    #[test]
    fn dns_config_validation_requires_kind_specific_fields() {
        let missing = DnsProviderConfig {
            kind: "namecheap".into(),
            token: None,
            api_user: Some("u".into()),
            api_key: None,
            username: Some("u".into()),
        };
        assert!(
            missing.validate().is_err(),
            "namecheap without api_key must fail"
        );
        let ok = DnsProviderConfig {
            kind: "hetzner".into(),
            token: Some("t".into()),
            api_user: None,
            api_key: None,
            username: None,
        };
        assert!(ok.validate().is_ok());
        let bad_kind = DnsProviderConfig {
            kind: "route53".into(),
            token: Some("t".into()),
            api_user: None,
            api_key: None,
            username: None,
        };
        assert!(bad_kind.validate().is_err());
    }

    #[test]
    fn legacy_cloudflare_token_still_deserializes() {
        let legacy: DnsProviderConfig =
            serde_json::from_str(r#"{"kind":"cloudflare","token":"cf_tok"}"#).unwrap();
        assert_eq!(legacy.kind, "cloudflare");
        assert_eq!(legacy.token.as_deref(), Some("cf_tok"));
    }

    #[test]
    fn acme_config_debug_redacts_the_token_through_the_nested_provider() {
        // `AcmeConfig` still derives `Debug`; this pins that the redaction
        // in `DnsProviderConfig` is enough on its own — no separate
        // hand-written impl is needed one level up.
        let cfg = AcmeConfig {
            email: "me@example.com".to_string(),
            control_hostname: None,
            provider: Some(cf("cf_topsecret_token")),
        };
        let dbg = format!("{cfg:?}");
        assert!(
            !dbg.contains("cf_topsecret_token"),
            "token leaked via Debug: {dbg}"
        );
    }
}
