# Project env store + deploy-config view — design

Two related dashboard capabilities, grouped by project:

1. **Hoster-managed environment.** Store env vars (secrets like Google API keys)
   in hoster per project, targeted at chosen services, injected into every
   branch deploy of that project — without baking them into the image or the
   repo's `hoster.json`.
2. **Deploy-config view.** Show the config a branch was deployed with (services,
   images, expose, env) so it's clear how a deployment came to be.

The dashboard is reorganized to **group deployments and env by project**.

## Decisions

| Question | Choice |
| --- | --- |
| Env scope / targeting | Per project, each var targets specific services (`services: []` = all). |
| Precedence vs `hoster.json` | Stored (hoster-managed) vars **win** on key conflict. |
| Secret display | Write-only, masked (`••••`) — values never returned by any read path. |
| Store persistence | Plain `0600` JSON file, atomic writes. |
| Deploy-config persistence | Docker labels (base64 of submitted config) — self-cleaning, restart-durable. |
| Config-view `hoster.json` env | Shown in plaintext (real secrets belong in the masked store). |
| Interfaces | Dashboard UI **and** bearer-token JSON API. |

## 1. The env store (`src/secrets.rs`)

File `/etc/hoster/projects.json` (override: `HOSTER_PROJECTS_FILE`), owned by the
`hoster` user, mode `0600`, written atomically (temp + rename):

```json
{
  "version": 1,
  "projects": {
    "odinvestor": {
      "vars": {
        "GOOGLE_API_KEY": { "value": "AIza…", "services": ["backend"] },
        "SENTRY_DSN":     { "value": "https://…", "services": [] }
      }
    }
  }
}
```

`services: []` → all services of the project; a non-empty list targets those
service names. Keys unique per project.

`Store` is thread-safe (`Arc` + inner `Mutex`) and persists on every mutation:

- `load(path) -> Store` — empty store if the file is absent.
- `set_var(project, key, value, services)` — upsert; validates inputs.
- `delete_var(project, key)`, `delete_project(project)`.
- `env_for(project, service) -> BTreeMap<String, String>` — vars whose target
  includes `service` (or is empty). Used by the engine at deploy.
- `list_masked() -> Vec<ProjectVars>` — projects → keys → `{ services, is_set:true }`.
  **Never** returns values.

Validation: env key matches `^[A-Za-z_][A-Za-z0-9_]*$`; each target service is a
DNS label; project name is a URL-safe label (must equal `project` in
`hoster.json`). Value length capped (e.g. 32 KiB).

## 2. Deploy-time merge (`src/engine.rs`)

`Engine` holds `Arc<Store>`. Per service, env is built as today —
`hoster.json` env with `{{…}}` substitution — then stored vars for
`(config.project, service)` are **overlaid verbatim** (no template
substitution), so a stored key overwrites the same key from `hoster.json` and a
`{{` inside a secret can never break a deploy.

Trust note: the secret does land in the target container's environment (visible
via `docker inspect` on the host) — inherent, consistent with hoster's
"host access = full access" model. It never appears in labels or logs.

## 3. Deploy-config persistence (`src/labels.rs`, `src/docker.rs`)

Two new labels written on every container at create time:

- `hoster.project` — the project name (grouping key).
- `hoster.config` — base64 of the **submitted** `hoster.json` (the injected
  store secrets are merged into the real env separately and are **not** in this
  label, so the config view cannot leak them).

`reconcile`/`list` read labels already; they group running containers by
`(project, branch)` and decode `hoster.config` from any one. Self-cleaning
(teardown removes containers → config gone) and restart-durable via the existing
reconcile path — no new stateful file for deploy history.

## 4. Dashboard, grouped by project (`src/dashboard.rs`, `src/api.rs`)

The dashboard renders a list of **projects**. Each project card shows:

- **Deployments** — its branches with status and URLs, each with an expandable
  **config** view: per service the image, `expose` (port/subdomain/health), and
  env. `hoster.json` env values render in plaintext; hoster-managed injected
  vars render **masked** and labelled "from hoster", so you see that a secret
  was injected and where, never its value.
- **Environment** — the project's stored vars (key + target services, value
  masked) with set / delete controls.

Projects appear if they have deployments or stored env (union of both sources).

### UI routes (cookie auth, dashboard-enabled)

- `GET /` — project-grouped dashboard.
- `POST /ui/projects/<project>/vars` — set/replace a var (form: key, value, services).
- `POST /ui/projects/<project>/vars/<key>/delete` — delete a var.
- `POST /ui/projects/<project>/delete` — delete all stored env for a project.
- existing `POST /ui/destroy/<branch>` — unchanged.

### JSON API (bearer token)

- `GET /projects` — masked list (projects, keys, target services). No values.
- `PUT /projects/<project>/vars/<key>` — body `{ "value": "...", "services": [...] }`.
- `DELETE /projects/<project>/vars/<key>`.

All under the existing bearer gate; GET never returns values.

## 5. Testing (TDD)

Store unit tests: set/get/delete, reload round-trip, atomic write leaves no
partial file, `env_for` targeting (`[]` = all, specific list, unknown service →
none), `list_masked` never leaks values, validation rejects bad keys/services/
oversized values.

Engine test: a stored var overrides `hoster.json` on key conflict and reaches
only targeted services; untargeted services don't receive it.

Label test: `hoster.config` round-trips through base64; grouping by
`(project, branch)` reconstructs the config; injected secrets are absent from
the decoded config.

Route tests (existing `tests/support` harness): UI set/delete flows; JSON API
get/set/delete; no endpoint returns a value; unauthorized requests rejected.

## 6. Docs

README + `docs/deploying.md`: a "Project environment & secrets" section
(setting vars, targeting, precedence over `hoster.json`, the `docker inspect`
trust note) and a note on the project-grouped dashboard and config view.

## Out of scope

Per-branch env overrides, encryption at rest, secret versioning/audit log,
non-secret (readable) vars. All revisitable later.
