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

/// Largest accepted value, guarding against a runaway paste filling the store.
pub const MAX_VALUE_LEN: usize = 32 * 1024;

/// One stored variable: its secret value and the services it targets. An empty
/// `services` list means every service in the project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Var {
    pub value: String,
    #[serde(default)]
    pub services: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectData {
    #[serde(default)]
    vars: BTreeMap<String, Var>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Data {
    version: u32,
    #[serde(default)]
    projects: BTreeMap<String, ProjectData>,
}

impl Default for Data {
    fn default() -> Self {
        Data {
            version: 1,
            projects: BTreeMap::new(),
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

/// A project's masked variables, for listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedProject {
    pub project: String,
    pub vars: Vec<MaskedVar>,
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
            return Err(format!(
                "env key {key:?} must match [A-Za-z_][A-Za-z0-9_]*"
            ));
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
        data.projects.entry(project.to_string()).or_default().vars.insert(
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
            if p.vars.is_empty() {
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
            })
            .collect()
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
        assert_eq!(env.get("GOOGLE_API_KEY").map(String::as_str), Some("AIza123"));
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
}
