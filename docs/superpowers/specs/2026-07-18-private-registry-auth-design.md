# Private registry authentication

**Date:** 2026-07-18
**Status:** Approved, not yet implemented

## Problem

`DockerRuntime::pull_image` (`src/docker.rs:72`) passes `None` as the
credentials argument to `create_image`. Every pull is therefore anonymous, and
any image in a private registry fails with a 401/403 at deploy time. Projects
whose code lives in a private repo — and whose CI pushes to a private registry —
cannot be deployed by hoster at all.

## Scope

Per-project registry credentials, managed through the existing dashboard and
control API, applied at pull time only to images whose registry host matches.

Explicitly out of scope: server-wide credentials, more than one credential per
project, credential verification at save time, encryption at rest beyond the
existing `0600` file mode.

## Design

### Storage

`src/secrets.rs` already owns the per-project store: a `0600` JSON file written
atomically, holding `projects: BTreeMap<String, ProjectData>`. `ProjectData`
gains one optional field alongside `vars`:

```rust
pub struct RegistryCred {
    pub registry: String,
    pub username: String,
    pub password: String,
}
```

The password follows the rule the store already applies to env-var values: it is
**never** returned through the read path used by the dashboard and API. A masked
form carries `{registry, username}` only.

The field is optional and `#[serde(default)]`, so the on-disk `version` stays at
`1`: a file written before this change simply loads with no credential. No
migration step.

### Runtime seam

The `Runtime` trait's `pull_image` gains a credential argument:

```rust
async fn pull_image(&self, image: &str, cred: Option<&RegistryCred>) -> anyhow::Result<()>;
```

`DockerRuntime` converts `RegistryCred` into `bollard::auth::DockerCredentials`
and passes it as `create_image`'s third argument, replacing the hardcoded
`None`. The fake runtime in `src/runtime.rs` records the credential it was
handed so engine tests can assert on it.

### Matching

A new pure function determines an image reference's registry host, applying
Docker's standard rules:

```rust
pub fn registry_host(image: &str) -> String
```

If the reference's first path segment contains `.` or `:`, or is exactly
`localhost`, that segment is the host. Otherwise the image belongs to Docker Hub
and the host is `docker.io`.

At deploy time the engine loads the project's credential and passes it to
`pull_image` **only** when `cred.registry == registry_host(image)`. A project
holding a `ghcr.io` token therefore still pulls `postgres:16` anonymously.

This function is the boundary that prevents a token from being sent to the wrong
registry, so it carries direct unit tests: bare name (`postgres`), user/name
(`library/postgres`), host with dot (`ghcr.io/org/app`), host with port
(`registry.internal:5000/app`), `localhost:5000/app`, and refs carrying a tag or
digest.

### API

Mirroring the existing project env-var endpoints, under the same token auth:

- `PUT /projects/{project}/registry` — body `{registry, username, password}`.
  Replaces any existing credential. `registry` and `username` must be non-empty;
  `password` is capped at the store's existing `MAX_VALUE_LEN`.
- `DELETE /projects/{project}/registry` — removes it. Idempotent.
- The existing project read endpoint includes the masked credential
  (`{registry, username}`) or null.

### Dashboard

One row in the existing per-project section: registry host and username shown as
text, password rendered as `••••`. A form sets or replaces the credential; a
button removes it. No new page, no new navigation.

### Error handling

Credentials are not verified when saved — no network call in the save path, and
configuring a project does not depend on the registry being reachable. A bad
credential surfaces at pull time: the registry's own error propagates through the
existing deploy-failure path into the deploy log.

## Testing

- `registry_host` unit tests, per the reference forms listed above.
- Store round-trip: set, read back masked, confirm the password is absent from
  the masked form; delete; load a file written without a credential.
- Engine: credential passed to `pull_image` when the host matches; `None` passed
  when it does not.
- API: set, masked read, delete, and rejection of empty `registry`/`username`.

## Accepted risks

1. **Plaintext at rest.** The password sits in `projects.json`, protected only by
   `0600` and the host's own security — identical to the treatment of existing
   project secrets. A registry PAT is often scoped more broadly than an app
   secret, so this is a real, if consistent, exposure.
2. **One credential per project.** A project pulling private images from two
   registries needs a schema change to a `BTreeMap<String, RegistryCred>`. The
   host-matching logic is written so that change is local to storage and lookup.
