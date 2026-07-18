# Private Registry Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let hoster pull images from a private registry using per-project credentials managed through the existing dashboard and control API.

**Architecture:** A `RegistryCred {registry, username, password}` is stored per project in the existing `0600` `projects.json` store. At deploy time the engine compares the credential's `registry` against the host parsed out of the image reference, and passes the credential to `pull_image` only on an exact match — so a `ghcr.io` token is never sent to Docker Hub. `DockerRuntime` forwards it to bollard's `create_image` as `DockerCredentials`.

**Tech Stack:** Rust, tokio, hyper, bollard (Docker API), serde. Tests are in-file `#[cfg(test)] mod tests` blocks, run with `cargo test`.

## Global Constraints

- The credential password is **never** returned through any read path — not the API, not the dashboard, not `list_masked`. Tests assert its absence by string search.
- The on-disk store `version` stays at `1`. The new field is `Option` + `#[serde(default)]`, so pre-existing `projects.json` files load unchanged. No migration.
- One credential per project. Storage is a single `Option<RegistryCred>`, not a map.
- Credentials are never verified at save time — no network call in any save path.
- Follow existing file conventions: doc comments on public items, tests in the same file, `anyhow::Result` for I/O errors, `Result<(), String>` for user-facing validation messages.

**Reference spec:** `docs/superpowers/specs/2026-07-18-private-registry-auth-design.md`

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/imageref.rs` | **Create** | Pure `registry_host()` — parse an image ref's registry host. The security boundary. |
| `src/lib.rs` | Modify | Register `pub mod imageref;` |
| `src/secrets.rs` | Modify | `RegistryCred`, store get/set/delete, masked form, `delete_var` pruning fix |
| `src/runtime.rs` | Modify | `pull_image` signature gains the credential; `FakeRuntime` records it |
| `src/docker.rs` | Modify | Map `RegistryCred` → `bollard::auth::DockerCredentials` |
| `src/engine.rs` | Modify | Host-match the credential and pass it to `pull_image` |
| `src/api.rs` | Modify | `PUT`/`DELETE /projects/{p}/registry`; dashboard form routes |
| `src/dashboard.rs` | Modify | Render the credential row and its set/remove forms |
| `README.md` | Modify | Document the feature under "Project environment & secrets" |

Tasks are ordered so each compiles and tests green on its own. Task 3 covers `runtime.rs`, `docker.rs`, and `engine.rs` together because changing a trait signature breaks every implementor at once — those three cannot compile separately.

---

### Task 1: Parse the registry host from an image reference

This is the function that stops a token being sent to the wrong registry. It gets tested first and hardest.

**Files:**
- Create: `src/imageref.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `pub fn registry_host(image: &str) -> String` — returns the lowercase registry host for an image reference, or `"docker.io"` for Docker Hub short forms.

**Rules (Docker's own):** split the reference at the first `/`. If there is no `/`, it's Docker Hub. If the first segment contains `.` or `:`, or is exactly `localhost`, that segment is the registry host. Otherwise it's a Docker Hub namespace (`library/postgres`), so the host is `docker.io`.

- [ ] **Step 1: Write the failing tests**

Create `src/imageref.rs`:

```rust
//! Parsing an image reference far enough to know which registry it comes from.
//!
//! This is a security boundary: a project's registry credential is sent only
//! when the host parsed here matches the credential's own registry, so a
//! private token never travels to Docker Hub on a `postgres:16` pull.

/// The registry host an image reference points at, applying Docker's rules:
/// the first path segment is the host only if it looks like one (contains `.`
/// or `:`, or is exactly `localhost`). Everything else is Docker Hub.
pub fn registry_host(image: &str) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_name_is_docker_hub() {
        assert_eq!(registry_host("postgres"), "docker.io");
        assert_eq!(registry_host("postgres:16"), "docker.io");
    }

    #[test]
    fn namespaced_name_is_docker_hub() {
        assert_eq!(registry_host("library/postgres"), "docker.io");
        assert_eq!(registry_host("bitnami/redis:7"), "docker.io");
    }

    #[test]
    fn dotted_first_segment_is_the_host() {
        assert_eq!(registry_host("ghcr.io/org/app"), "ghcr.io");
        assert_eq!(registry_host("ghcr.io/org/app:v1"), "ghcr.io");
        assert_eq!(registry_host("registry.gitlab.com/g/p/img"), "registry.gitlab.com");
    }

    #[test]
    fn host_with_port_keeps_the_port() {
        assert_eq!(registry_host("registry.internal:5000/app"), "registry.internal:5000");
    }

    #[test]
    fn localhost_is_a_host_without_a_dot() {
        assert_eq!(registry_host("localhost/app"), "localhost");
        assert_eq!(registry_host("localhost:5000/app:tag"), "localhost:5000");
    }

    #[test]
    fn digest_refs_parse_the_same() {
        assert_eq!(
            registry_host("ghcr.io/org/app@sha256:abc123"),
            "ghcr.io"
        );
        assert_eq!(registry_host("postgres@sha256:abc123"), "docker.io");
    }

    #[test]
    fn host_is_lowercased() {
        assert_eq!(registry_host("GHCR.IO/org/app"), "ghcr.io");
    }

    #[test]
    fn empty_and_malformed_refs_do_not_panic() {
        assert_eq!(registry_host(""), "docker.io");
        assert_eq!(registry_host("/leading-slash"), "docker.io");
    }
}
```

- [ ] **Step 2: Register the module**

In `src/lib.rs`, add alongside the existing `pub mod` lines (keep them alphabetical if they already are):

```rust
pub mod imageref;
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib imageref`
Expected: FAIL — the tests panic at `not yet implemented` from the `todo!()`.

- [ ] **Step 4: Implement**

Replace the `todo!()` body in `src/imageref.rs`:

```rust
pub fn registry_host(image: &str) -> String {
    const DOCKER_HUB: &str = "docker.io";
    let Some((first, _rest)) = image.split_once('/') else {
        return DOCKER_HUB.to_string();
    };
    if first.is_empty() {
        return DOCKER_HUB.to_string();
    }
    if first.contains('.') || first.contains(':') || first == "localhost" {
        first.to_ascii_lowercase()
    } else {
        DOCKER_HUB.to_string()
    }
}
```

Note `localhost:5000/app` is caught by the `:` check, and bare `localhost/app` by the explicit comparison.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib imageref`
Expected: PASS — 8 tests.

- [ ] **Step 6: Commit**

```bash
git add src/imageref.rs src/lib.rs
git commit -m "feat: parse the registry host from an image reference"
```

---

### Task 2: Store the credential

**Files:**
- Modify: `src/secrets.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub struct RegistryCred { pub registry: String, pub username: String, pub password: String }` (derives `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`)
  - `pub struct MaskedRegistry { pub registry: String, pub username: String }` (derives `Debug, Clone, PartialEq, Eq, Serialize`)
  - `Store::set_registry(&self, project: &str, registry: &str, username: &str, password: &str) -> Result<(), String>`
  - `Store::delete_registry(&self, project: &str) -> anyhow::Result<()>`
  - `Store::registry_for(&self, project: &str) -> Option<RegistryCred>`
  - `MaskedProject` gains `pub registry: Option<MaskedRegistry>`

**Bug to fix in this task:** `delete_var` currently removes the whole project entry when `vars` becomes empty. Once a project can hold a credential with no vars, that silently deletes the credential. The prune must also require the credential to be absent.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block at the bottom of `src/secrets.rs`:

```rust
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
        s.set_registry("p", "ghcr.io", "bot", "ghp_topsecret").unwrap();
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
        assert_eq!(s.env_for("p", "backend").get("K").map(String::as_str), Some("v"));
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
        assert!(s.set_registry("bad/project", "ghcr.io", "bot", "x").is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib secrets`
Expected: FAIL to compile — `no method named set_registry found for struct Store`.

- [ ] **Step 3: Add the types**

In `src/secrets.rs`, after the `Var` struct:

```rust
/// A project's container-registry credential. One per project; applied at pull
/// time only to images whose host matches `registry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryCred {
    pub registry: String,
    pub username: String,
    pub password: String,
}
```

Add the field to `ProjectData`:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ProjectData {
    #[serde(default)]
    vars: BTreeMap<String, Var>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    registry: Option<RegistryCred>,
}
```

Next to `MaskedVar`, add the masked form:

```rust
/// A registry credential as exposed to the UI/API: host and username,
/// **never** the password.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedRegistry {
    pub registry: String,
    pub username: String,
}
```

And extend `MaskedProject`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MaskedProject {
    pub project: String,
    pub vars: Vec<MaskedVar>,
    pub registry: Option<MaskedRegistry>,
}
```

- [ ] **Step 4: Add the store methods**

In `impl Store`, after `delete_project`:

```rust
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
        data.projects.entry(project.to_string()).or_default().registry = Some(RegistryCred {
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
            if p.vars.is_empty() {
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
```

The host is lowercased on the way in so it compares equal to `registry_host`'s lowercased output.

- [ ] **Step 5: Fix the `delete_var` pruning bug**

In `delete_var`, the prune must not discard a credential. Change:

```rust
            if p.vars.is_empty() {
```

to:

```rust
            if p.vars.is_empty() && p.registry.is_none() {
```

- [ ] **Step 6: Include the credential in the masked listing**

In `list_masked`, add the field to the constructed `MaskedProject`:

```rust
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
            })
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib secrets`
Expected: PASS — the pre-existing secrets tests plus the 13 new ones.

If `dashboard.rs` tests fail to compile here because its test helper builds a `MaskedProject` literal, that is expected and fixed in Task 6. Confirm with `cargo test --lib secrets` scoped as above; leave the rest for Task 6.

- [ ] **Step 8: Commit**

```bash
git add src/secrets.rs
git commit -m "feat: store a per-project registry credential"
```

---

### Task 3: Thread the credential through the runtime and engine

The `ContainerRuntime` trait signature, both implementors, and the engine call site must change together — a trait change breaks every implementor at once, so they cannot compile as separate tasks.

**Files:**
- Modify: `src/runtime.rs`, `src/docker.rs`, `src/engine.rs`

**Interfaces:**
- Consumes: `registry_host` (Task 1), `RegistryCred` and `Store::registry_for` (Task 2).
- Produces:
  - `ContainerRuntime::pull_image(&self, image: &str, cred: Option<&RegistryCred>) -> anyhow::Result<()>`
  - `FakeRuntime::pull_cred_of(&self, image: &str) -> Option<Option<RegistryCred>>` — outer `None` means the image was never pulled; inner `None` means it was pulled anonymously.

- [ ] **Step 1: Write the failing tests**

In `src/runtime.rs`, append to `mod tests`:

```rust
    #[tokio::test]
    async fn fake_runtime_records_the_pull_credential() {
        let rt = FakeRuntime::new();
        let cred = crate::secrets::RegistryCred {
            registry: "ghcr.io".into(),
            username: "bot".into(),
            password: "x".into(),
        };
        rt.pull_image("ghcr.io/org/app:v1", Some(&cred)).await.unwrap();
        rt.pull_image("postgres:16", None).await.unwrap();

        assert_eq!(rt.pull_cred_of("ghcr.io/org/app:v1"), Some(Some(cred)));
        assert_eq!(rt.pull_cred_of("postgres:16"), Some(None));
        assert_eq!(rt.pull_cred_of("never-pulled"), None);
    }
```

In `src/engine.rs`, append to `mod tests`. These are the tests that prove the security boundary end to end — read the existing deploy tests around line 490 first and mirror their setup helper exactly (same `Engine` construction, same `DeployRequest` shape).

```rust
    #[tokio::test]
    async fn credential_is_sent_only_to_the_matching_registry() {
        // A project with a ghcr.io credential deploying two services: one
        // private image from ghcr.io, one public image from Docker Hub.
        let (engine, runtime, store) = engine_with_fake();
        store
            .set_registry("myproj", "ghcr.io", "bot", "ghp_secret")
            .unwrap();

        let config = r#"{"project":"myproj","services":{
            "postgres":{"image":"postgres:16"},
            "backend":{"image":"ghcr.io/org/backend:v1","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        let sent = runtime.pull_cred_of("ghcr.io/org/backend:v1").unwrap();
        assert_eq!(
            sent.as_ref().map(|c| c.username.as_str()),
            Some("bot"),
            "credential should be sent to its own registry"
        );

        let public = runtime.pull_cred_of("postgres:16").unwrap();
        assert!(
            public.is_none(),
            "ghcr.io credential must NOT be sent to Docker Hub"
        );
    }

    #[tokio::test]
    async fn no_credential_stored_means_anonymous_pulls() {
        let (engine, runtime, _store) = engine_with_fake();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"ghcr.io/org/backend:v1","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();
        assert_eq!(runtime.pull_cred_of("ghcr.io/org/backend:v1"), Some(None));
    }
```

If helpers named `engine_with_fake` and `deploy_config` do not already exist in `src/engine.rs`'s test module, write them as thin wrappers over whatever setup the existing tests use, returning `(Engine<FakeRuntime>, Arc<FakeRuntime>, Arc<Store>)` and performing one deploy respectively. Do not duplicate setup inline in each test.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib`
Expected: FAIL to compile — `pull_image` takes 1 argument but 2 were supplied; `no method named pull_cred_of`.

- [ ] **Step 3: Change the trait and the fake**

In `src/runtime.rs`, add the import at the top:

```rust
use crate::secrets::RegistryCred;
```

Change the trait method:

```rust
    async fn pull_image(&self, image: &str, cred: Option<&RegistryCred>) -> anyhow::Result<()>;
```

Add a field to `FakeState`:

```rust
    /// Credential passed to `pull_image`, keyed by image ref — for test
    /// assertions that a token reached only its own registry.
    pull_creds: BTreeMap<String, Option<RegistryCred>>,
```

Add the accessor in `impl FakeRuntime`:

```rust
    /// What `pull_image` was given for an image: `None` if it was never
    /// pulled, `Some(None)` if pulled anonymously, `Some(Some(c))` if
    /// authenticated.
    pub fn pull_cred_of(&self, image: &str) -> Option<Option<RegistryCred>> {
        self.inner.lock().unwrap().pull_creds.get(image).cloned()
    }
```

And record it in the fake's implementation:

```rust
    async fn pull_image(&self, image: &str, cred: Option<&RegistryCred>) -> anyhow::Result<()> {
        self.inner
            .lock()
            .unwrap()
            .pull_creds
            .insert(image.to_string(), cred.cloned());
        Ok(())
    }
```

- [ ] **Step 4: Pass the credential to Docker**

In `src/docker.rs`, add the imports:

```rust
use bollard::auth::DockerCredentials;
use crate::secrets::RegistryCred;
```

Replace `pull_image` (currently at `src/docker.rs:72`):

```rust
    async fn pull_image(&self, image: &str, cred: Option<&RegistryCred>) -> anyhow::Result<()> {
        let auth = cred.map(|c| DockerCredentials {
            username: Some(c.username.clone()),
            password: Some(c.password.clone()),
            serveraddress: Some(c.registry.clone()),
            ..Default::default()
        });
        self.docker
            .create_image(
                Some(CreateImageOptions {
                    from_image: image.to_string(),
                    ..Default::default()
                }),
                None,
                auth,
            )
            .try_collect::<Vec<_>>()
            .await?;
        Ok(())
    }
```

- [ ] **Step 5: Match the host in the engine**

In `src/engine.rs`, add the import:

```rust
use crate::imageref::registry_host;
```

Inside the service loop, replace the pull call (currently `self.runtime.pull_image(&image).await?;` at `src/engine.rs:268`):

```rust
            // Send the project's credential only to its own registry — a
            // ghcr.io token must never travel to Docker Hub on a public pull.
            let cred = self
                .store
                .registry_for(project)
                .filter(|c| c.registry == registry_host(&image));
            self.runtime.pull_image(&image, cred.as_ref()).await?;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, except any `dashboard.rs` test that constructs a `MaskedProject` literal — Task 6 fixes those. If such a compile error blocks the run, add `registry: None` to that test helper's literal now and leave the rendering work for Task 6.

- [ ] **Step 7: Verify the whole crate builds clean**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings. (If clippy is not installed, `cargo build --all-targets` is the fallback.)

- [ ] **Step 8: Commit**

```bash
git add src/runtime.rs src/docker.rs src/engine.rs
git commit -m "feat: authenticate image pulls with the project's registry credential"
```

---

### Task 4: Control API endpoints

**Files:**
- Modify: `src/api.rs`

**Interfaces:**
- Consumes: `Store::set_registry`, `Store::delete_registry` (Task 2).
- Produces: `PUT /projects/{project}/registry`, `DELETE /projects/{project}/registry`. The existing `GET /projects` gains the masked credential automatically via `list_masked`.

- [ ] **Step 1: Write the failing tests**

Append to `mod tests` in `src/api.rs`, mirroring the setup the existing var-endpoint tests use (find them by searching for `/vars/` in that module and copy their harness — same engine construction, same bearer header helper):

```rust
    #[tokio::test]
    async fn put_registry_stores_the_credential() {
        let (engine, settings, sessions) = api_harness();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/registry",
            r#"{"registry":"ghcr.io","username":"bot","password":"ghp_secret"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let c = engine.store().registry_for("myproj").unwrap();
        assert_eq!(c.registry, "ghcr.io");
        assert_eq!(c.password, "ghp_secret");
    }

    #[tokio::test]
    async fn get_projects_masks_the_registry_password() {
        let (engine, settings, sessions) = api_harness();
        engine
            .store()
            .set_registry("myproj", "ghcr.io", "bot", "ghp_topsecret")
            .unwrap();
        let res = call(&engine, &settings, &sessions, Method::GET, "/projects", "").await;
        let body = body_string(res).await;
        assert!(!body.contains("ghp_topsecret"), "password leaked: {body}");
        assert!(body.contains("ghcr.io"));
        assert!(body.contains("bot"));
    }

    #[tokio::test]
    async fn delete_registry_removes_the_credential() {
        let (engine, settings, sessions) = api_harness();
        engine
            .store()
            .set_registry("myproj", "ghcr.io", "bot", "x")
            .unwrap();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::DELETE,
            "/projects/myproj/registry",
            "",
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(engine.store().registry_for("myproj").is_none());
    }

    #[tokio::test]
    async fn put_registry_rejects_an_empty_username() {
        let (engine, settings, sessions) = api_harness();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/registry",
            r#"{"registry":"ghcr.io","username":"","password":"x"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn registry_endpoints_require_the_bearer_token() {
        let (engine, settings, sessions) = api_harness();
        let res = call_without_token(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/registry",
            r#"{"registry":"ghcr.io","username":"bot","password":"x"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
```

Name the harness helpers to match whatever the existing tests already use; if they inline their setup, extract `api_harness`, `call`, `call_without_token`, and `body_string` once and let the existing tests keep working unchanged.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib api`
Expected: FAIL — the PUT returns 404 (`not found`), because no route matches yet.

- [ ] **Step 3: Add the path parser and body type**

In `src/api.rs`, next to `parse_var_path`:

```rust
/// Extract `<project>` from `/projects/<project>/registry`, or `None` if the
/// path isn't that shape.
fn parse_registry_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/projects/")?;
    let project = rest.strip_suffix("/registry")?;
    if project.is_empty() || project.contains('/') {
        return None;
    }
    Some(project.to_string())
}

/// The body of `PUT /projects/<project>/registry`.
#[derive(Debug, Deserialize)]
struct SetRegistryBody {
    registry: String,
    username: String,
    password: String,
}
```

- [ ] **Step 4: Add the handlers**

Next to `handle_set_var`:

```rust
async fn handle_set_registry<R: ContainerRuntime>(
    req: Request<Incoming>,
    engine: &Engine<R>,
    project: String,
) -> Result<Response<ApiBody>, Infallible> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(text(StatusCode::BAD_REQUEST, "could not read request body")),
    };
    let body: SetRegistryBody = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            return Ok(text_owned(
                StatusCode::BAD_REQUEST,
                format!("invalid request body: {e}"),
            ));
        }
    };
    match engine
        .store()
        .set_registry(&project, &body.registry, &body.username, &body.password)
    {
        Ok(()) => Ok(text(StatusCode::NO_CONTENT, "")),
        Err(msg) => Ok(text_owned(StatusCode::BAD_REQUEST, msg)),
    }
}

fn handle_delete_registry<R: ContainerRuntime>(
    engine: &Engine<R>,
    project: &str,
) -> Response<ApiBody> {
    let _ = engine.store().delete_registry(project);
    text(StatusCode::NO_CONTENT, "")
}
```

- [ ] **Step 5: Route them**

In `handle_api`'s bearer-token `match`, add these arms **before** the `parse_var_path` arms (the paths are disjoint, but keeping the more specific suffix first avoids surprises if `parse_var_path` is ever loosened):

```rust
        (Method::PUT, p) if parse_registry_path(p).is_some() => {
            let project = parse_registry_path(p).unwrap();
            handle_set_registry(req, &engine, project).await
        }
        (Method::DELETE, p) if parse_registry_path(p).is_some() => {
            let project = parse_registry_path(p).unwrap();
            Ok(handle_delete_registry(&engine, &project))
        }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib api`
Expected: PASS — 5 new tests plus the existing API tests.

- [ ] **Step 7: Commit**

```bash
git add src/api.rs
git commit -m "feat: control API endpoints for the project registry credential"
```

---

### Task 5: Dashboard UI

**Files:**
- Modify: `src/dashboard.rs`, `src/api.rs`

**Interfaces:**
- Consumes: `MaskedProject::registry`, `MaskedRegistry` (Task 2); `Store::set_registry`, `Store::delete_registry`.
- Produces: `POST /ui/projects/<project>/registry` (set) and `POST /ui/projects/<project>/registry/delete` (remove), both redirecting to `/`.

- [ ] **Step 1: Write the failing tests**

In `src/dashboard.rs`'s `mod tests`, first update the `masked` helper so it compiles against the new struct — add the field and a second helper:

```rust
    fn masked(project: &str, vars: &[(&str, &[&str])]) -> MaskedProject {
        // ...existing body, with this added to the constructed literal:
        //     registry: None,
    }

    fn masked_with_registry(project: &str, registry: &str, username: &str) -> MaskedProject {
        MaskedProject {
            project: project.to_string(),
            vars: vec![],
            registry: Some(MaskedRegistry {
                registry: registry.to_string(),
                username: username.to_string(),
            }),
        }
    }
```

Then add the tests:

```rust
    #[test]
    fn registry_row_shows_host_and_username_but_masks_the_password() {
        let env = [masked_with_registry("p", "ghcr.io", "bot")];
        let html = dashboard_page(&[], &env);
        assert!(html.contains("ghcr.io"));
        assert!(html.contains("bot"));
        assert!(html.contains("••••"));
        assert!(html.contains("/ui/projects/p/registry/delete"));
    }

    #[test]
    fn project_without_a_registry_shows_the_empty_state_and_a_form() {
        let env = [masked("p", &[("K", &[][..])])];
        let html = dashboard_page(&[], &env);
        assert!(html.contains("No registry credential"));
        assert!(html.contains("action=\"/ui/projects/p/registry\""));
    }

    #[test]
    fn registry_fields_are_html_escaped() {
        let env = [masked_with_registry("p", "ghcr.io", "<script>x</script>")];
        let html = dashboard_page(&[], &env);
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib dashboard`
Expected: FAIL — assertion failures on the missing markup (after the `masked` helper fix makes it compile).

- [ ] **Step 3: Render the credential**

In `src/dashboard.rs`, import `MaskedRegistry` alongside the existing `MaskedProject` import, and add this function next to `render_environment`:

```rust
/// The registry-credential block for one project: the stored host and
/// username (password masked, never rendered) plus a set/replace form.
fn render_registry(body: &mut String, project: &str, env: &[MaskedProject]) {
    body.push_str("<h3>Registry credential</h3>");
    let cred = env
        .iter()
        .find(|p| p.project == project)
        .and_then(|p| p.registry.as_ref());
    match cred {
        None => body.push_str("<p class=\"empty\">No registry credential. Public images only.</p>"),
        Some(c) => {
            let _ = write!(
                body,
                "<table><thead><tr><th>Registry</th><th>Username</th><th>Password</th><th></th></tr></thead>\
<tbody><tr><td><code>{registry}</code></td><td>{username}</td><td class=\"muted\">••••••</td>\
<td><form method=\"post\" action=\"/ui/projects/{proj}/registry/delete\" \
onsubmit=\"return confirm('Remove this registry credential?')\">\
<button class=\"destroy\" type=\"submit\">Remove</button></form></td></tr></tbody></table>",
                registry = html_escape(&c.registry),
                username = html_escape(&c.username),
                proj = html_escape(project),
            );
        }
    }
    let _ = write!(
        body,
        "<form class=\"addvar\" method=\"post\" action=\"/ui/projects/{proj}/registry\">\
<input name=\"registry\" placeholder=\"ghcr.io\" required>\
<input name=\"username\" placeholder=\"username\" required>\
<input name=\"password\" type=\"password\" placeholder=\"token or password\" required>\
<button type=\"submit\">Save credential</button></form>",
        proj = html_escape(project),
    );
}
```

Call it from `dashboard_page` immediately after the existing `render_environment(...)` call, with the same arguments.

- [ ] **Step 4: Handle the form posts**

In `src/api.rs`'s `ui_projects`, add these two blocks **before** the existing `strip_suffix("/vars")` block (the `/delete` suffix check that follows would otherwise swallow `/registry/delete`):

```rust
    if let Some(project) = sub.strip_suffix("/registry/delete") {
        let _ = engine.store().delete_registry(project);
        return redirect("/");
    }

    if let Some(project) = sub.strip_suffix("/registry") {
        let project = project.to_string();
        let bytes = match req.into_body().collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return text(StatusCode::BAD_REQUEST, "could not read request body"),
        };
        let registry = form_field(&bytes, "registry").unwrap_or_default();
        let username = form_field(&bytes, "username").unwrap_or_default();
        let password = form_field(&bytes, "password").unwrap_or_default();
        return match engine
            .store()
            .set_registry(&project, &registry, &username, &password)
        {
            Ok(()) => redirect("/"),
            Err(msg) => text_owned(StatusCode::BAD_REQUEST, msg),
        };
    }
```

Order matters: `/registry/delete` must be tested before `/registry`, since `strip_suffix("/registry")` would not match it but the generic `/delete` handler below would mis-route it to `delete_project`.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test`
Expected: PASS — everything, including the previously-deferred dashboard tests.

- [ ] **Step 6: Verify clean**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: no output, exit 0.

- [ ] **Step 7: Commit**

```bash
git add src/dashboard.rs src/api.rs
git commit -m "feat: manage the registry credential from the dashboard"
```

---

### Task 6: Document it

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add the section**

In `README.md`, directly after the existing "Project environment & secrets" section, add a sibling section and its entry in the Contents list (`- [Private registry credentials](#private-registry-credentials)`):

```markdown
### Private registry credentials

If your images live in a private registry, give the project a credential and
hoster authenticates its pulls.

In the dashboard, each project has a **Registry credential** block: enter the
registry host, a username, and a token. For GitHub Container Registry that's
`ghcr.io`, your GitHub username, and a personal access token with `read:packages`.

Or through the API:

```bash
curl -X PUT https://hoster.example.com/projects/myproj/registry \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"registry":"ghcr.io","username":"my-user","password":"ghp_..."}'
```

Remove it with `DELETE /projects/myproj/registry`.

The credential is used **only** for images whose registry host matches it. A
project holding a `ghcr.io` token still pulls `postgres:16` anonymously from
Docker Hub, so the token never leaves the registry it belongs to.

One credential per project. The password is stored in the projects file
(`0600`, same as project env vars) and is never shown again — the dashboard and
API return the host and username only. Credentials are not verified when saved;
a bad one shows up as a failed deploy with the registry's own error.
```

- [ ] **Step 2: Verify the docs match reality**

Re-read the section against `src/api.rs`'s routes and `src/secrets.rs`'s validation. Every path, method, and field name must match the code exactly.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document private registry credentials"
```

---

## Final verification

- [ ] `cargo test` — all tests pass
- [ ] `cargo clippy --all-targets -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] `grep -rn "pull_image" src/` — every call site passes a credential argument
- [ ] Manual end-to-end check against a real private registry, since no test exercises the bollard auth path: store a real credential, deploy a branch whose image is private, confirm the pull succeeds; then delete the credential, redeploy, and confirm it fails with the registry's 401.
