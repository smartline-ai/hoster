# Multi-domain routing

**Date:** 2026-07-19
**Status:** Approved, not yet implemented
**Branch:** `networking`

## Problem

`Settings::hostname_template` is a single global string (`HOSTER_HOSTNAME_TEMPLATE`,
default `{service}-{branch}.dev.example.com`). Every branch of every project is
therefore forced onto one domain. An operator who wants `dev.example.com` for
development branches, `demo.example.com` for demos, and separate domains per
client (`dev.example1.com`, `dev.example2.com`) cannot express that.

This is also a prerequisite for automatic TLS: hoster cannot obtain a
certificate for a domain its routing layer does not know about.

## Scope

Per-project hostname templates, managed through the existing dashboard and
control API, resolved at deploy time.

Explicitly out of scope, and deliberately deferred to later slices:

- Built-in TLS termination and ACME certificate issuance.
- DNS provider integrations (writing records automatically).
- Per-deploy or per-environment domain selection — the model here is
  per-project.

**What this slice does not remove:** the operator still adds an nginx server
block and a wildcard certificate by hand for each new domain. This slice makes
hoster *capable* of serving several domains; the TLS/ACME slice is what removes
the manual step.

## Design

### Storage

The per-project store (`src/secrets.rs`, backed by the `0600` `projects.json`)
already holds each project's env vars and registry credential. `ProjectData`
gains one optional field:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
hostname_template: Option<String>,
```

`None` means the project inherits the global `HOSTER_HOSTNAME_TEMPLATE`. The
field is `Option` + `#[serde(default)]`, so the on-disk `version` stays at `1`
and files written before this change load unchanged. No migration.

Unlike the registry password, the template is not a secret: it is returned in
full through the read paths.

The store's project-pruning conditions must account for the new field. Both
`delete_var` and `delete_registry` prune a project entry once it holds nothing;
each must also require `hostname_template.is_none()`, and the new
`delete_hostname_template` must require `vars.is_empty() && registry.is_none()`.
Otherwise removing one kind of data silently discards another.

### Resolution

`Engine` gains one private helper:

```rust
fn template_for(&self, project: &str) -> String
```

returning the project's stored template, or `self.settings.hostname_template`
when it has none.

The four existing call sites in `src/engine.rs` — lines 145, 266, 334, and 372,
each currently `hostname_for(&self.settings.hostname_template, …)` — become
`hostname_for(&self.template_for(project), …)`. That is the entire behavioural
change.

### Why routing, the proxy, and reconciliation need no changes

The resolved hostname is already written into the `hoster.hostname` container
label at deploy time, and `src/labels.rs` rebuilds the routing table by reading
`hoster.hostname`, `hoster.port`, and the container IP off running containers.
The routing table is keyed by hostname string; it neither knows nor cares which
template produced it.

Two consequences follow, both desirable:

- A running container carries its own URL. Restart recovery works unchanged.
- Changing a project's template affects only *subsequent* deploys. Branches
  already running keep their existing hostnames until redeployed, rather than
  being silently orphaned from their routes.

### Validation

`Store::set_hostname_template` validates before storing, returning
`Result<(), String>` with a human-readable message on the first violation —
the same convention as `set_registry`:

- The project name is valid (existing `is_project_name`).
- The template is non-empty.
- The template contains `{branch}`. Without it, every branch of the project
  resolves to one hostname and each deploy silently displaces the previous —
  a guaranteed break, distinct from the accepted cross-project collision case
  below. `{service}` is **not** required: `{branch}.demo.example.com` is a
  legitimate single-service pattern.
- Substituting sample values for `{service}` and `{branch}` yields a valid DNS
  name: each label 1–63 characters, characters limited to `[a-z0-9-]` with no
  leading or trailing hyphen per label, total length ≤253.

### Accepted: cross-project collisions

Two projects may be given the same template. This is permitted deliberately.
A collision only materialises when both projects expose a same-named service on
a same-named branch, in which case the later deploy wins — the behaviour that
exists today. Rejecting shared domains would block the legitimate case of two
projects with distinct service names sharing one domain.

### API

Mirroring the registry-credential endpoints, under the same bearer-token gate:

- `PUT /projects/{project}/domain` — body `{"hostname_template": "..."}`.
  Replaces any existing value. `400` with the validation message on rejection.
- `DELETE /projects/{project}/domain` — reverts the project to the global
  default. Idempotent.
- The existing project read path includes the template, or `null` when the
  project inherits the default.

### Dashboard

One row in the existing per-project panel, in the same visual language as the
Environment and Registry credential panels: the effective domain, either the
project's own template or the global default explicitly marked as inherited, a
form to set or replace it, and a button to revert to the default. Values are
HTML-escaped.

## Testing

- `template_for` returns the project's template when set and the global default
  when not.
- Validation rejections: empty, `{branch}` missing, an over-long label, invalid
  characters. Acceptance: `{branch}.demo.example.com` (no `{service}`).
- Store round-trip: set, read back, delete, and load a `projects.json` written
  without the field.
- Pruning: a project holding only a template survives `delete_var` and
  `delete_registry`; a project holding vars survives `delete_hostname_template`.
- Engine: two projects with different templates produce different
  `hoster.hostname` labels from the same branch name.
- Reconciliation: a running container keeps its original hostname after its
  project's template changes — proving routing is rebuilt from labels, not
  recomputed.
- API: set, read, delete, validation rejection, and rejection without a token.
- Dashboard: the inherited-default state and the overridden state both render;
  values are escaped.

## Consequences for the next slice

Once a project can name its own domain, the set of domains hoster must serve
becomes enumerable from the store: the distinct templates across all projects,
plus the global default. That set is exactly the input the ACME slice needs to
decide which wildcard certificates to obtain.
