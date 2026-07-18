# Multi-Domain Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let each project serve its branches on its own domain, instead of every project sharing one global hostname template.

**Architecture:** The per-project store gains an optional `hostname_template`. The engine resolves a project's template at deploy time, falling back to the global `HOSTER_HOSTNAME_TEMPLATE`. Because the resolved hostname is already written into the `hoster.hostname` container label, and routing is rebuilt from labels, the proxy and reconciliation paths need no changes at all.

**Tech Stack:** Rust, tokio, hyper, serde. Tests are in-file `#[cfg(test)] mod tests` blocks, run with `cargo test`.

**Worktree:** `/Users/pavel/Projects/hoster-networking`, branch `networking`. All commands run from that directory.

## Global Constraints

- The on-disk store `version` stays at `1`. The new field is `Option` + `#[serde(default)]`, so pre-existing `projects.json` files load unchanged. No migration code.
- The hostname template is **not** a secret — it is returned in full through every read path, unlike the registry password.
- A template must contain `{branch}`. `{service}` is optional.
- Project pruning must never discard one kind of project data when another is deleted.
- Changing a project's template must not disturb branches already running — their hostnames live in container labels.
- Clean under `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
- Follow existing file conventions: doc comments on public items, tests in the same file, `anyhow::Result` for I/O errors, `Result<(), String>` for user-facing validation messages.

**Reference spec:** `docs/superpowers/specs/2026-07-19-multi-domain-routing-design.md`

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `src/settings.rs` | Modify | `validate_hostname_template` — the pure validation rules |
| `src/secrets.rs` | Modify | Store the template; get/set/delete; pruning fix |
| `src/engine.rs` | Modify | `template_for`; collapse three duplicated URL loops into one |
| `src/api.rs` | Modify | `PUT`/`DELETE /projects/{p}/domain`; dashboard form route |
| `src/dashboard.rs` | Modify | Render the effective domain and its forms |
| `README.md` | Modify | Document per-project domains |

Task 3 deliberately includes a small refactor: `deploy`, `plan_urls`, and `urls_for` currently contain three byte-identical URL-building loops. Threading `project` through three copies would triple the change surface, so Task 3 collapses them into one call site first.

---

### Task 1: Validate a hostname template

**Files:**
- Modify: `src/settings.rs`

**Interfaces:**
- Produces: `pub fn validate_hostname_template(template: &str) -> Result<(), String>` — `Ok(())` if the template is usable, else a human-readable message naming the problem.

**Rules:** non-empty; contains `{branch}`; **all placeholders confined to the template's first label**; and substituting sample values yields a valid DNS name — total length ≤253, each dot-separated label 1–63 characters, each label made only of `[a-z0-9-]` and neither starting nor ending with `-`.

The first-label rule exists because a TLS wildcard matches exactly one label. `{service}-{branch}.dev.example.com` reduces to `*.dev.example.com`, but `{branch}.{service}.dev.example.com` would need `*.*.dev.example.com`, which Let's Encrypt will not issue. Enforcing it here means the certificate slice can always derive a usable wildcard.

Uppercase is rejected rather than silently lowercased, so what the operator stores is exactly what gets served.

- [ ] **Step 1: Write the failing tests**

Append to the existing `mod tests` block in `src/settings.rs`:

```rust
    #[test]
    fn accepts_a_normal_template() {
        assert!(validate_hostname_template("{service}-{branch}.dev.example.com").is_ok());
    }

    #[test]
    fn accepts_a_template_without_service() {
        assert!(validate_hostname_template("{branch}.demo.example.com").is_ok());
    }

    #[test]
    fn rejects_an_empty_template() {
        assert!(validate_hostname_template("").is_err());
    }

    #[test]
    fn rejects_a_template_without_branch() {
        let err = validate_hostname_template("{service}.dev.example.com").unwrap_err();
        assert!(err.contains("{branch}"), "message should name the missing placeholder: {err}");
    }

    #[test]
    fn rejects_placeholders_spanning_two_labels() {
        // A TLS wildcard matches one label, so this could never be covered by
        // a certificate for *.dev.example.com.
        let err = validate_hostname_template("{branch}.{service}.dev.example.com").unwrap_err();
        assert!(
            err.contains("first label"),
            "message should explain the one-label rule: {err}"
        );
    }

    #[test]
    fn rejects_a_placeholder_outside_the_first_label() {
        assert!(validate_hostname_template("api.{branch}.dev.example.com").is_err());
    }

    #[test]
    fn rejects_uppercase() {
        assert!(validate_hostname_template("{branch}.Dev.Example.com").is_err());
    }

    #[test]
    fn rejects_an_underscore() {
        assert!(validate_hostname_template("{branch}.dev_example.com").is_err());
    }

    #[test]
    fn rejects_an_empty_label() {
        assert!(validate_hostname_template("{branch}..example.com").is_err());
    }

    #[test]
    fn rejects_a_leading_or_trailing_hyphen_in_a_label() {
        assert!(validate_hostname_template("{branch}.-example.com").is_err());
        assert!(validate_hostname_template("{branch}.example-.com").is_err());
    }

    #[test]
    fn rejects_an_over_long_label() {
        let long = "a".repeat(64);
        assert!(validate_hostname_template(&format!("{{branch}}.{long}.com")).is_err());
    }

    #[test]
    fn accepts_a_label_of_exactly_63() {
        let ok = "a".repeat(63);
        assert!(validate_hostname_template(&format!("{{branch}}.{ok}.com")).is_ok());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib settings`
Expected: FAIL to compile — `cannot find function validate_hostname_template in this scope`.

- [ ] **Step 3: Implement**

Add to `src/settings.rs`, below `hostname_for`:

```rust
/// Sample values substituted for the placeholders when validating a template.
/// Short and legal, so any length or character failure the check reports comes
/// from the operator's own text rather than from the sample.
const SAMPLE_SERVICE: &str = "svc";
const SAMPLE_BRANCH: &str = "br";

/// Check that a hostname template is usable before storing it.
///
/// Requires `{branch}` — without it every branch of the project resolves to one
/// hostname and each deploy silently displaces the previous. `{service}` is
/// optional: `{branch}.demo.example.com` is a legitimate single-service pattern.
pub fn validate_hostname_template(template: &str) -> Result<(), String> {
    if template.is_empty() {
        return Err("hostname template must not be empty".to_string());
    }
    if !template.contains("{branch}") {
        return Err(
            "hostname template must contain {branch}, or every branch of the project \
would resolve to the same hostname"
                .to_string(),
        );
    }
    // A TLS wildcard matches exactly one label, so every placeholder must sit
    // in the first label for `*.<rest>` to cover the hostnames produced here.
    let first_label = template.split('.').next().unwrap_or("");
    let rest = &template[first_label.len().min(template.len())..];
    if rest.contains('{') {
        return Err(
            "every placeholder must be in the hostname template's first label, \
because a TLS wildcard certificate matches only one label"
                .to_string(),
        );
    }
    let sample = hostname_for(template, SAMPLE_SERVICE, SAMPLE_BRANCH);
    validate_dns_name(&sample)
}

/// Validate a concrete hostname: total length, label lengths, and the
/// characters permitted in a DNS label.
fn validate_dns_name(name: &str) -> Result<(), String> {
    if name.len() > 253 {
        return Err(format!(
            "hostname {name:?} is {} characters; the maximum is 253",
            name.len()
        ));
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err(format!(
                "hostname {name:?} has an empty label (check for a doubled or trailing '.')"
            ));
        }
        if label.len() > 63 {
            return Err(format!(
                "label {label:?} is {} characters; the maximum is 63",
                label.len()
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("label {label:?} must not start or end with '-'"));
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(format!(
                "label {label:?} contains {bad:?}; only lowercase letters, digits, and '-' are allowed"
            ));
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib settings`
Expected: PASS — the 3 pre-existing settings tests plus the 10 new ones.

- [ ] **Step 5: Verify clean**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: no output, exit 0.

- [ ] **Step 6: Commit**

```bash
git add src/settings.rs
git commit -m "feat: validate hostname templates"
```

---

### Task 2: Store a per-project hostname template

**Files:**
- Modify: `src/secrets.rs`

**Interfaces:**
- Consumes: `crate::settings::validate_hostname_template` (Task 1).
- Produces:
  - `Store::set_hostname_template(&self, project: &str, template: &str) -> Result<(), String>`
  - `Store::delete_hostname_template(&self, project: &str) -> anyhow::Result<()>`
  - `Store::hostname_template_for(&self, project: &str) -> Option<String>`
  - `MaskedProject` gains `pub hostname_template: Option<String>`

**Pruning:** `ProjectData` is dropped once a project holds nothing. With a third kind of data, every deletion path must check all three, or deleting one silently discards another.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/secrets.rs`:

```rust
    #[test]
    fn set_then_hostname_template_for_returns_it() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
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
        s.set_hostname_template("p", "{branch}.a.example.com").unwrap();
        s.set_hostname_template("p", "{branch}.b.example.com").unwrap();
        assert_eq!(
            s.hostname_template_for("p").as_deref(),
            Some("{branch}.b.example.com")
        );
    }

    #[test]
    fn delete_hostname_template_removes_it_and_is_idempotent() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
        s.delete_hostname_template("p").unwrap();
        assert!(s.hostname_template_for("p").is_none());
        s.delete_hostname_template("p").unwrap();
    }

    #[test]
    fn set_hostname_template_rejects_an_invalid_template() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_hostname_template("p", "{service}.example.com").is_err());
        assert!(s.set_hostname_template("p", "").is_err());
        assert!(s.hostname_template_for("p").is_none(), "nothing should be stored on rejection");
    }

    #[test]
    fn set_hostname_template_rejects_an_invalid_project_name() {
        let s = Store::load(temp_file()).unwrap();
        assert!(s.set_hostname_template("bad/project", "{branch}.example.com").is_err());
    }

    #[test]
    fn project_with_only_a_hostname_template_is_listed() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
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
        s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.delete_var("p", "K").unwrap();
        assert!(s.hostname_template_for("p").is_some(), "template pruned with the last var");
    }

    #[test]
    fn deleting_the_registry_keeps_the_hostname_template() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_registry("p").unwrap();
        assert!(s.hostname_template_for("p").is_some(), "template pruned with the registry");
    }

    #[test]
    fn deleting_the_hostname_template_keeps_vars_and_registry() {
        let s = Store::load(temp_file()).unwrap();
        s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
        s.set_var("p", "K", "v", vec![]).unwrap();
        s.set_registry("p", "ghcr.io", "bot", "x").unwrap();
        s.delete_hostname_template("p").unwrap();
        assert_eq!(s.env_for("p", "backend").get("K").map(String::as_str), Some("v"));
        assert!(s.registry_for("p").is_some());
    }

    #[test]
    fn hostname_template_persists_and_reloads_from_disk() {
        let path = temp_file();
        {
            let s = Store::load(&path).unwrap();
            s.set_hostname_template("p", "{branch}.demo.example.com").unwrap();
        }
        let s2 = Store::load(&path).unwrap();
        assert_eq!(
            s2.hostname_template_for("p").as_deref(),
            Some("{branch}.demo.example.com")
        );
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
        assert_eq!(s.env_for("p", "backend").get("K").map(String::as_str), Some("v"));
        assert!(s.hostname_template_for("p").is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib secrets`
Expected: FAIL to compile — `no method named set_hostname_template found for struct Store`.

- [ ] **Step 3: Add the field**

In `src/secrets.rs`, add to `ProjectData` alongside `vars` and `registry`:

```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hostname_template: Option<String>,
```

And to `MaskedProject`:

```rust
    pub hostname_template: Option<String>,
```

- [ ] **Step 4: Add the store methods**

Add the import at the top of `src/secrets.rs`:

```rust
use crate::settings::validate_hostname_template;
```

In `impl Store`, after `delete_registry`:

```rust
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
```

- [ ] **Step 5: Fix both existing pruning conditions**

In `delete_var`, change:

```rust
            if p.vars.is_empty() && p.registry.is_none() {
```

to:

```rust
            if p.vars.is_empty() && p.registry.is_none() && p.hostname_template.is_none() {
```

In `delete_registry`, change:

```rust
            if p.vars.is_empty() {
```

to:

```rust
            if p.vars.is_empty() && p.hostname_template.is_none() {
```

- [ ] **Step 6: Include the template in the masked listing**

In `list_masked`, add to the constructed `MaskedProject`:

```rust
                hostname_template: p.hostname_template.clone(),
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib secrets`
Expected: PASS.

If `src/dashboard.rs`'s test module fails to compile because it builds a `MaskedProject` literal, add `hostname_template: None` to that literal so the crate compiles. The dashboard's rendering work belongs to Task 5.

- [ ] **Step 8: Commit**

```bash
git add src/secrets.rs src/dashboard.rs
git commit -m "feat: store a per-project hostname template"
```

---

### Task 3: Resolve the template per project

Three URL-building loops in `src/engine.rs` are byte-identical. This task collapses them into one call site, then makes that one site project-aware.

**Files:**
- Modify: `src/engine.rs`

**Interfaces:**
- Consumes: `Store::hostname_template_for` (Task 2).
- Produces:
  - `Engine::template_for(&self, project: &str) -> String` (private)
  - `Engine::urls_for(&self, services: &BTreeMap<String, config::Service>, branch: &str, project: &str) -> BTreeMap<String, String>` (private; gains the `project` parameter)

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/engine.rs`. Read the existing tests first and reuse their setup helpers (`engine_with_fake` / `deploy_config` / `engine_with_store` — use whichever the file actually has; do not duplicate setup inline):

```rust
    #[tokio::test]
    async fn a_projects_template_overrides_the_global_default() {
        let (engine, runtime, store) = engine_with_fake();
        store
            .set_hostname_template("myproj", "{service}-{branch}.demo.example.com")
            .unwrap();

        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.name.ends_with("backend"))
            .expect("backend container");
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-main.demo.example.com"
        );
    }

    #[tokio::test]
    async fn a_project_without_a_template_uses_the_global_default() {
        let (engine, runtime, _store) = engine_with_fake();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.name.ends_with("backend"))
            .expect("backend container");
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-main.dev.example.com"
        );
    }

    #[tokio::test]
    async fn two_projects_get_different_hostnames_for_the_same_branch() {
        let (engine, runtime, store) = engine_with_fake();
        store.set_hostname_template("alpha", "{service}-{branch}.a.example.com").unwrap();
        store.set_hostname_template("beta", "{service}-{branch}.b.example.com").unwrap();

        deploy_config(
            &engine,
            "main",
            r#"{"project":"alpha","services":{"backend":{"image":"img","expose":{"port":8080}}}}"#,
        )
        .await
        .unwrap();
        deploy_config(
            &engine,
            "release",
            r#"{"project":"beta","services":{"backend":{"image":"img","expose":{"port":8080}}}}"#,
        )
        .await
        .unwrap();

        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let hosts: Vec<&str> = containers
            .iter()
            .filter_map(|c| c.labels.get(crate::labels::HOSTNAME))
            .map(String::as_str)
            .collect();
        assert!(hosts.contains(&"backend-main.a.example.com"), "got {hosts:?}");
        assert!(hosts.contains(&"backend-release.b.example.com"), "got {hosts:?}");
    }

    #[tokio::test]
    async fn deploy_urls_use_the_projects_template() {
        let (engine, _runtime, store) = engine_with_fake();
        store
            .set_hostname_template("myproj", "{service}-{branch}.demo.example.com")
            .unwrap();
        let req = request(
            "main",
            r#"{"project":"myproj","services":{"backend":{"image":"img","expose":{"port":8080}}}}"#,
        );
        let urls = engine.plan_urls(&req);
        assert_eq!(
            urls.get("backend").map(String::as_str),
            Some("http://backend-main.demo.example.com")
        );
    }

    #[tokio::test]
    async fn a_running_branch_keeps_its_hostname_after_its_template_changes() {
        let (engine, runtime, store) = engine_with_fake();
        store.set_hostname_template("myproj", "{service}-{branch}.old.example.com").unwrap();
        let config = r#"{"project":"myproj","services":{
            "backend":{"image":"img","expose":{"port":8080}}
        }}"#;
        deploy_config(&engine, "main", config).await.unwrap();

        // Change the template without redeploying.
        store.set_hostname_template("myproj", "{service}-{branch}.new.example.com").unwrap();

        // Views are rebuilt from container labels, so the running branch keeps
        // the hostname it was deployed with.
        let containers = runtime.list_by_label(crate::labels::BRANCH).await.unwrap();
        let backend = containers
            .iter()
            .find(|c| c.name.ends_with("backend"))
            .expect("backend container");
        assert_eq!(
            backend.labels[crate::labels::HOSTNAME],
            "backend-main.old.example.com",
            "a running container's hostname must not change under it"
        );
    }
```

If the helper returning `(Engine, Arc<FakeRuntime>, Arc<Store>)` does not exist, write it as a thin wrapper over the setup the existing tests already use, and leave those tests unchanged.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib engine`
Expected: FAIL — hostnames come out as `backend-main.dev.example.com` regardless of the project's stored template.

- [ ] **Step 3: Add the resolver**

In `src/engine.rs`, add to `impl Engine`, next to the other private helpers:

```rust
    /// The hostname template for `project`: its own if it has one, otherwise
    /// the operator's global default.
    fn template_for(&self, project: &str) -> String {
        self.store
            .hostname_template_for(project)
            .unwrap_or_else(|| self.settings.hostname_template.clone())
    }
```

- [ ] **Step 4: Give `urls_for` the project, and collapse the duplicate loops**

Change `urls_for` to take a project and use its template:

```rust
    /// Compute the public URLs for a service map on a branch — the URL of every
    /// exposed service. Deterministic from config + branch + the project's
    /// template, so it works for views reconstructed from labels after a
    /// restart.
    fn urls_for(
        &self,
        services: &BTreeMap<String, config::Service>,
        branch: &str,
        project: &str,
    ) -> BTreeMap<String, String> {
        let template = self.template_for(project);
        let mut urls = BTreeMap::new();
        for (name, svc) in services {
            if let Some(exp) = &svc.expose {
                let sub = exp.subdomain.clone().unwrap_or_else(|| name.clone());
                let host = hostname_for(&template, &sub, branch);
                urls.insert(name.clone(), format!("http://{host}"));
            }
        }
        urls
    }
```

In `deploy`, replace the URL-building loop (the block at roughly lines 140–147 that begins `// 1. hostnames + urls for exposed services`) with a single call:

```rust
        // 1. hostnames + urls for exposed services
        let urls = self.urls_for(&req.config.services, &branch, &req.config.project);
```

In `plan_urls`, replace its whole body after the `branch` binding with:

```rust
    pub fn plan_urls(&self, req: &DeployRequest) -> BTreeMap<String, String> {
        let branch = sanitize_branch(&req.branch);
        self.urls_for(&req.config.services, &branch, &req.config.project)
    }
```

In `deployment_views`, the existing call becomes:

```rust
                .map(|c| self.urls_for(&c.services, &branch, &project))
```

`project` is still owned at that point and is only moved into `DeploymentView` further down, so a borrow here is fine.

- [ ] **Step 5: Make the container label project-aware**

In the service loop that builds container labels, replace:

```rust
                    hostname_for(&self.settings.hostname_template, &sub, branch),
```

with:

```rust
                    hostname_for(&self.template_for(project), &sub, branch),
```

`project: &str` is already a parameter of that function.

- [ ] **Step 6: Confirm no call site was missed**

Run: `grep -n 'settings.hostname_template' src/engine.rs`
Expected: no output. Every resolution now goes through `template_for`.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --lib engine`
Expected: PASS — the pre-existing engine tests plus the 5 new ones.

- [ ] **Step 8: Run the full suite**

Run: `cargo test`
Expected: PASS. This task changed a shared helper, so the whole suite matters.

- [ ] **Step 9: Commit**

```bash
git add src/engine.rs
git commit -m "feat: resolve hostnames from the project's own template"
```

---

### Task 4: Control API endpoints

**Files:**
- Modify: `src/api.rs`

**Interfaces:**
- Consumes: `Store::set_hostname_template`, `Store::delete_hostname_template` (Task 2).
- Produces: `PUT /projects/{project}/domain`, `DELETE /projects/{project}/domain`. `GET /projects` gains the template automatically through `list_masked`.

- [ ] **Step 1: Write the failing tests**

Append to the `mod tests` block in `src/api.rs`, reusing the harness helpers the registry tests already use (`api_harness`, `call`, `call_without_token`, `body_string` — read them first and reuse, do not duplicate):

```rust
    #[tokio::test]
    async fn put_domain_stores_the_template() {
        let (engine, settings, sessions) = api_harness();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/domain",
            r#"{"hostname_template":"{branch}.demo.example.com"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            engine.store().hostname_template_for("myproj").as_deref(),
            Some("{branch}.demo.example.com")
        );
    }

    #[tokio::test]
    async fn get_projects_includes_the_template() {
        let (engine, settings, sessions) = api_harness();
        engine
            .store()
            .set_hostname_template("myproj", "{branch}.demo.example.com")
            .unwrap();
        let res = call(&engine, &settings, &sessions, Method::GET, "/projects", "").await;
        let body = body_string(res).await;
        assert!(body.contains("demo.example.com"), "body: {body}");
    }

    #[tokio::test]
    async fn delete_domain_reverts_to_the_default() {
        let (engine, settings, sessions) = api_harness();
        engine
            .store()
            .set_hostname_template("myproj", "{branch}.demo.example.com")
            .unwrap();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::DELETE,
            "/projects/myproj/domain",
            "",
        )
        .await;
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        assert!(engine.store().hostname_template_for("myproj").is_none());
    }

    #[tokio::test]
    async fn put_domain_rejects_a_template_without_branch() {
        let (engine, settings, sessions) = api_harness();
        let res = call(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/domain",
            r#"{"hostname_template":"{service}.demo.example.com"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(engine.store().hostname_template_for("myproj").is_none());
    }

    #[tokio::test]
    async fn domain_endpoints_require_the_bearer_token() {
        let (engine, settings, sessions) = api_harness();
        let res = call_without_token(
            &engine,
            &settings,
            &sessions,
            Method::PUT,
            "/projects/myproj/domain",
            r#"{"hostname_template":"{branch}.demo.example.com"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib api`
Expected: FAIL — the PUT returns 404, no route matches.

- [ ] **Step 3: Add the path parser and body type**

In `src/api.rs`, next to `parse_registry_path`:

```rust
/// Extract `<project>` from `/projects/<project>/domain`, or `None` if the
/// path isn't that shape.
fn parse_domain_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/projects/")?;
    let project = rest.strip_suffix("/domain")?;
    if project.is_empty() || project.contains('/') {
        return None;
    }
    Some(project.to_string())
}

/// The body of `PUT /projects/<project>/domain`.
#[derive(Debug, Deserialize)]
struct SetDomainBody {
    hostname_template: String,
}
```

- [ ] **Step 4: Add the handlers**

Next to `handle_set_registry`:

```rust
async fn handle_set_domain<R: ContainerRuntime>(
    req: Request<Incoming>,
    engine: &Engine<R>,
    project: String,
) -> Result<Response<ApiBody>, Infallible> {
    let bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Ok(text(StatusCode::BAD_REQUEST, "could not read request body")),
    };
    let body: SetDomainBody = match serde_json::from_slice(&bytes) {
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
        .set_hostname_template(&project, &body.hostname_template)
    {
        Ok(()) => Ok(text(StatusCode::NO_CONTENT, "")),
        Err(msg) => Ok(text_owned(StatusCode::BAD_REQUEST, msg)),
    }
}

fn handle_delete_domain<R: ContainerRuntime>(
    engine: &Engine<R>,
    project: &str,
) -> Response<ApiBody> {
    let _ = engine.store().delete_hostname_template(project);
    text(StatusCode::NO_CONTENT, "")
}
```

- [ ] **Step 5: Route them**

In `handle_api`'s bearer-token `match`, add these arms alongside the registry arms:

```rust
        (Method::PUT, p) if parse_domain_path(p).is_some() => {
            let project = parse_domain_path(p).unwrap();
            handle_set_domain(req, &engine, project).await
        }
        (Method::DELETE, p) if parse_domain_path(p).is_some() => {
            let project = parse_domain_path(p).unwrap();
            Ok(handle_delete_domain(&engine, &project))
        }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib api`
Expected: PASS — 5 new tests plus the existing API tests.

- [ ] **Step 7: Commit**

```bash
git add src/api.rs
git commit -m "feat: control API endpoints for the project domain"
```

---

### Task 5: Dashboard

**Files:**
- Modify: `src/dashboard.rs`, `src/api.rs`

**Interfaces:**
- Consumes: `MaskedProject::hostname_template` (Task 2); `Store::set_hostname_template`, `Store::delete_hostname_template`.
- Produces: `POST /ui/projects/<project>/domain` and `POST /ui/projects/<project>/domain/delete`, both redirecting to `/`.

**Design language:** match the existing panels. Read `render_environment` and `render_registry` first and follow their structure — `<aside class="col …">` wrappers, `<div class="col-label">`, `<div class="empty">` empty states, `env-row`-style rows, `<button class="icon-btn">` for destructive actions, `<form class="add-var">` for the input form. Do not introduce a class that has no style rule behind it.

**Behaviour:** show the *effective* domain. When the project has its own template, show it with a control to remove it (reverting to the default). When it does not, show the global default explicitly marked as inherited. `dashboard_page` will need the global default passed in — see Step 3.

- [ ] **Step 1: Write the failing tests**

In `src/dashboard.rs`'s `mod tests`, add `hostname_template: None` to the existing `masked` helper's literal, then add:

```rust
    fn masked_with_template(project: &str, template: &str) -> MaskedProject {
        MaskedProject {
            project: project.to_string(),
            vars: vec![],
            registry: None,
            hostname_template: Some(template.to_string()),
        }
    }

    #[test]
    fn shows_a_projects_own_domain_with_a_reset_control() {
        let env = [masked_with_template("p", "{branch}.demo.example.com")];
        let html = dashboard_page(&[], &env, "{service}-{branch}.dev.example.com");
        assert!(html.contains("demo.example.com"));
        assert!(html.contains("/ui/projects/p/domain/delete"));
    }

    #[test]
    fn shows_the_global_default_as_inherited_when_unset() {
        let env = [masked("p", &[("K", &[][..])])];
        let html = dashboard_page(&[], &env, "{service}-{branch}.dev.example.com");
        assert!(html.contains("dev.example.com"));
        assert!(
            html.to_lowercase().contains("default"),
            "an inherited domain should be labelled as the default"
        );
        assert!(html.contains("action=\"/ui/projects/p/domain\""));
    }

    #[test]
    fn domain_is_html_escaped() {
        let env = [masked_with_template("p", "{branch}.<script>x</script>.com")];
        let html = dashboard_page(&[], &env, "{service}-{branch}.dev.example.com");
        assert!(!html.contains("<script>x</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib dashboard`
Expected: FAIL to compile — `dashboard_page` takes 2 arguments but 3 were supplied.

- [ ] **Step 3: Thread the global default into the page**

Change the signature:

```rust
pub fn dashboard_page(
    deployments: &[DeploymentView],
    env: &[MaskedProject],
    default_template: &str,
) -> String {
```

Update the existing call site in `src/api.rs` (in `ui_root`) to pass `&settings.hostname_template`. Update any pre-existing `dashboard_page` calls in tests to pass `"{service}-{branch}.dev.example.com"`.

- [ ] **Step 4: Render the panel**

Add to `src/dashboard.rs`, next to `render_registry`, and call it from `dashboard_page` alongside the other per-project panels:

```rust
/// The domain block for one project: the effective hostname template — the
/// project's own, or the global default marked as inherited — plus a form to
/// set or replace it.
fn render_domain(
    body: &mut String,
    project: &str,
    env: &[MaskedProject],
    default_template: &str,
) {
    let own = env
        .iter()
        .find(|p| p.project == project)
        .and_then(|p| p.hostname_template.as_deref());
    let proj = html_escape(project);

    body.push_str("<div class=\"col-label\">Domain</div>");
    match own {
        None => {
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<div class=\"env-meta\"><span class=\"tag all\">default</span></div></div>",
                html_escape(default_template),
            );
        }
        Some(t) => {
            let _ = write!(
                body,
                "<div class=\"env-row\"><span class=\"k\">{}</span>\
<form method=\"post\" action=\"/ui/projects/{proj}/domain/delete\" \
onsubmit=\"return confirm('Revert this project to the default domain?')\">\
<button class=\"icon-btn\" type=\"submit\" title=\"Revert to default\">\u{2715}</button></form>\
</div>",
                html_escape(t),
            );
        }
    }
    let _ = write!(
        body,
        "<form class=\"add-var\" method=\"post\" action=\"/ui/projects/{proj}/domain\">\
<input name=\"hostname_template\" placeholder=\"{{branch}}.demo.example.com\" required>\
<button type=\"submit\">Save domain</button></form>",
    );
}
```

- [ ] **Step 5: Handle the form posts**

In `src/api.rs`'s `ui_projects`, add these blocks **before** the existing `/registry` blocks, keeping `/domain/delete` ahead of `/domain`:

```rust
    if let Some(project) = sub.strip_suffix("/domain/delete") {
        let _ = engine.store().delete_hostname_template(project);
        return redirect("/");
    }

    if let Some(project) = sub.strip_suffix("/domain") {
        let project = project.to_string();
        let bytes = match req.into_body().collect().await {
            Ok(c) => c.to_bytes(),
            Err(_) => return text(StatusCode::BAD_REQUEST, "could not read request body"),
        };
        let template = form_field(&bytes, "hostname_template").unwrap_or_default();
        return match engine.store().set_hostname_template(&project, &template) {
            Ok(()) => redirect("/"),
            Err(msg) => text_owned(StatusCode::BAD_REQUEST, msg),
        };
    }
```

Order matters: `/domain/delete` must be tested before `/domain`, and both before the generic `/delete` handler, or a revert is misrouted into `delete_project` and wipes the project's env vars.

- [ ] **Step 6: Add a routing regression test**

Append to `src/api.rs`'s `mod tests`, following the cookie-authenticated dashboard tests already there:

```rust
    #[tokio::test]
    async fn ui_domain_delete_does_not_wipe_the_projects_vars() {
        let (engine, settings, sessions) = api_harness();
        engine.store().set_var("p", "KEEP", "v", vec![]).unwrap();
        engine
            .store()
            .set_hostname_template("p", "{branch}.demo.example.com")
            .unwrap();

        let _ = ui_post(&engine, &settings, &sessions, "/ui/projects/p/domain/delete").await;

        assert!(engine.store().hostname_template_for("p").is_none());
        assert_eq!(
            engine.store().env_for("p", "backend").get("KEEP").map(String::as_str),
            Some("v"),
            "reverting the domain must not delete the project's variables"
        );
    }
```

Use whatever cookie-authenticated POST helper the file already has; if none exists, write one thin helper rather than inlining the session setup.

- [ ] **Step 7: Run the full suite**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 8: Verify clean**

Run: `cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: no output, exit 0.

- [ ] **Step 9: Commit**

```bash
git add src/dashboard.rs src/api.rs
git commit -m "feat: manage the project domain from the dashboard"
```

---

### Task 6: Document it

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Verify every claim against the code**

Before writing, confirm in the source: the exact route paths and methods (`src/api.rs`), the JSON body field name, the validation rules actually enforced (`src/settings.rs`), and what the dashboard panel shows (`src/dashboard.rs`). Correct the draft below wherever it drifted from what the code does.

- [ ] **Step 2: Add the section**

In `README.md`, after the "Private registry credentials" section, add a sibling section, and add `- [Per-project domains](#per-project-domains)` to the Contents list matching its neighbours' nesting:

```markdown
### Per-project domains

By default every branch of every project lands on `HOSTER_HOSTNAME_TEMPLATE`.
A project can override that with its own template, so one hoster can serve
`dev.example.com` for one project and `demo.example.com` for another.

In the dashboard, each project's **Domain** panel shows its effective template —
either its own, or the global default marked as the default — with a form to
change it.

Or through the API:

```bash
curl -fsS -X PUT $API/projects/myproj/domain \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"hostname_template":"{branch}.demo.example.com"}'
```

`DELETE /projects/myproj/domain` reverts the project to the global default.

The template must contain `{branch}` — without it, every branch of the project
would resolve to one hostname and each deploy would displace the previous.
`{service}` is optional, so `{branch}.demo.example.com` works for a
single-service project.

Changing a project's domain affects **subsequent** deploys only. Branches
already running keep the hostnames they were deployed with, because each
container records its own hostname; redeploy a branch to move it.

Each domain still needs its own wildcard DNS record, and — until hoster
terminates TLS itself — its own certificate and reverse-proxy server block.
```

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document per-project domains"
```

---

## Final verification

- [ ] `cargo test` — all tests pass
- [ ] `cargo clippy --all-targets -- -D warnings` — clean
- [ ] `cargo fmt --check` — clean
- [ ] `grep -rn 'settings.hostname_template' src/` — appears only in `main.rs` (constructing Settings), `api.rs` (passing the default to the dashboard), and `engine.rs`'s `template_for` fallback
- [ ] Render the dashboard and confirm the Domain panel shows both states — a project with its own domain, and one inheriting the default
