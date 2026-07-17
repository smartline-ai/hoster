# hoster deploy engine — automatic per-branch deploys via Docker

**Status:** design approved (autonomous execution authorized), building
**Date:** 2026-07-17
**Builds on:** the proxy-core milestone (merged) and
`2026-07-17-hoster-design.md`.

## Goal

Replace the hand-written routes file with automatic deployment. CI calls one
endpoint with a branch, an image tag, and the branch's `hoster.json`; hoster
brings up that branch's whole stack on an isolated Docker network, wires
service-name DNS, learns each container's IP, and swaps the proxy's routing
table — all with no manual step. `DELETE` tears a branch down.

## What changed from the original design

The original design drove **containerd** directly and planned to build CNI
networking and an embedded DNS server by hand. This milestone drives the
**Docker API** (via the `bollard` crate) instead, because Docker provides the
two hardest pieces — per-branch isolated networks and automatic service-name
DNS — for free. Docker runs on containerd underneath, so the runtime choice is
preserved; we simply stop re-implementing Docker on top of it. This also makes
the engine reachable from macOS during development (Docker Desktop / Colima /
OrbStack expose a socket), which raw containerd does not.

## Scope

**In:** `hoster.json` config model + validation, template substitution, a
`ContainerRuntime` abstraction with a real Docker implementation and a fake,
the deploy engine (full-replace orchestration), the control API
(`POST /deploy`, `DELETE /deploy/{branch}`, `GET /deployments`), shared-token
auth, and startup reconciliation of the routing table from Docker labels.

**Deferred (later milestones, unchanged from the master design):** TLS/ACME,
TTL/reaping, the dashboard, per-project tokens (this milestone uses one shared
token), and SQLite (Docker labels are the source of truth for now). The proxy
continues to serve plain HTTP.

## Two decisions this milestone makes

**State lives in Docker labels, not a database.** Every container hoster
creates is labelled with its branch, service, exposed port, and public
hostname. Docker itself is the source of truth: on startup hoster lists its
labelled containers and rebuilds the routing table. No SQLite until there is
persistent state (TTL, audit) that genuinely needs it. This removes a whole
subsystem from this milestone and means a hoster restart recovers routing with
zero extra machinery.

**One shared bearer token guards the control API.** Not wide open, but not the
per-project token system yet (that needs the DB). One line of operator config
now; per-project tokens arrive with SQLite later.

## Architecture

The proxy from milestone one is unchanged and untouched. This milestone adds a
control plane beside it. They meet at the same `SharedRoutes` the proxy already
reads lock-free.

```
     CI ──POST /deploy {branch,tag,config}──┐  (shared-token bearer auth)
                                            ▼
                              ┌──────────── api ────────────┐
                              │  validate token, parse body │
                              └──────────────┬──────────────┘
                                             ▼
        ┌──────────────────────────── engine ───────────────────────────┐
        │ validate config → substitute templates → full-replace:         │
        │   teardown branch → create network → pull+run each service     │
        │   → inspect IPs → readiness-gate → build RoutingTable → swap    │
        └───────────────┬───────────────────────────────┬───────────────┘
                        │ ContainerRuntime trait         │ SharedRoutes::swap
                        ▼                                 ▼
              DockerRuntime (bollard)            ArcSwap<RoutingTable>
                        │                                 │ reads
                        ▼                          ┌──────▼──────┐
                  Docker daemon                    │ proxy (:public) │
              per-branch bridge networks           └─────────────┘
              service-name DNS, container IPs
```

Two listeners: the **proxy** on the public port (as today), and the **control
API** on a separate, private port. The API is never in the routing table and
never publicly routed — it is reached directly on its own port.

## Modules

New files, each one responsibility:

| File | Responsibility |
| --- | --- |
| `src/config.rs` | `hoster.json` model (`DeployConfig`, `Service`, `Expose`), `deny_unknown_fields`, validation. Pure. |
| `src/template.rs` | Template variable substitution in image refs and env values. Pure. |
| `src/runtime.rs` | `ContainerRuntime` trait + plain types (`ContainerSpec`, `RunningContainer`, `RuntimeError`). The seam. |
| `src/docker.rs` | `DockerRuntime`: the trait implemented over `bollard`. Integration-tested against a live socket. |
| `src/labels.rs` | Label key constants and the container→route reconciliation mapping. Pure. |
| `src/engine.rs` | Deploy/teardown orchestration over the trait. Unit-tested with a fake runtime. |
| `src/api.rs` | Control HTTP API + shared-token auth. |
| `src/settings.rs` | Operator config (public/api listen addrs, hostname template, registry base, docker connection, shared token). |

Deleted: `src/routes_file.rs` and `routes.example.toml` (the scaffolding this
milestone replaces).

The `ContainerRuntime` trait is the load-bearing boundary: the engine's whole
orchestration is testable against an in-memory fake, so none of the deploy
logic requires a running Docker to test. Only `docker.rs` touches bollard, and
only its tests need a socket.

## `hoster.json`

```json
{
  "project": "odinvestor",
  "services": {
    "postgres": {
      "image": "postgres:16",
      "env": { "POSTGRES_PASSWORD": "dev", "POSTGRES_DB": "app" }
    },
    "backend": {
      "image": "{{registry}}/backend:{{tag}}",
      "env": {
        "DATABASE_URL": "postgres://postgres:dev@postgres:5432/app",
        "PUBLIC_URL": "{{url.backend}}"
      },
      "expose": { "port": 8080, "health": "/healthz" }
    },
    "frontend": {
      "image": "{{registry}}/frontend:{{tag}}",
      "env": { "API_URL": "{{url.backend}}" },
      "expose": { "port": 3000 }
    }
  }
}
```

- `expose` absent ⇒ internal-only: a container and a service-name DNS entry,
  no hostname, no route. Default-closed, as in the master design.
- `expose.port` is the container port. `expose.health` is an optional HTTP path
  used for readiness gating; absent ⇒ a TCP connect check is used instead.
- `ttl` is accepted and ignored this milestone (reaping is deferred); accepting
  it now keeps configs forward-compatible.

## Template variables

Substituted at deploy time, in `image` and `env` values only:

| var | value |
| --- | --- |
| `{{registry}}` | operator `registry_base` |
| `{{tag}}` | tag from the deploy request |
| `{{branch}}` | sanitized branch label |
| `{{sha}}` | sha from the deploy request (empty string if omitted) |
| `{{url.<service>}}` | full public URL of an exposed service; validation error if `<service>` is absent or not exposed |

`{{url.*}}` is for browser-facing values only; container-to-container traffic
uses service-name DNS (`postgres:5432`), never a public URL.

## Deploy flow

`POST /deploy`, `Authorization: Bearer <shared-token>`:

```json
{ "branch": "feature/JIRA-123", "tag": "a1b2c3d", "sha": "a1b2c3d…",
  "config": { …hoster.json… } }
```

1. **Auth**: constant-time compare the bearer token; 401 on mismatch.
2. **Validate** the config: unknown fields, empty services, `{{url.x}}` → an
   unexposed/absent service, illegal service names, illegal resulting DNS
   labels. All rejection happens here, before anything is created. 400 on
   failure.
3. **Sanitize** the branch into a DNS label (`feature/JIRA-123` →
   `feature-jira-123`).
4. **Compute hostnames** for exposed services from the operator
   `hostname_template`, then **substitute** templates in every image and env
   value (so `{{url.backend}}` resolves).
5. **Full replace**: remove every container and the network labelled
   `hoster.branch=<branch>`.
6. **Create** the per-branch bridge network `hoster-<branch>`.
7. **Pull and run** each service on that network, container named `<branch>-
   <service>`, labelled `hoster.branch`, `hoster.service`, and — for exposed
   services — `hoster.port` and `hoster.hostname`.
8. **Inspect** each exposed container to read its IP on the branch network.
9. **Readiness-gate**: for each exposed service, poll its `health` HTTP path
   (2xx–4xx = ready) or, absent a path, a TCP connect, until ready or a
   timeout. On timeout the deploy fails.
10. **Build** a `RoutingTable` from the exposed services' hostnames → IP:port
    and **swap** it into `SharedRoutes`.

Steps 5–10 run in a background task; the request returns `202 Accepted` with
the computed URLs and a `provisioning` status as soon as validation passes, so
CI is not held open for image pulls. `GET /deployments` reports current state.

**Full replace, ephemeral everything** (as the master design chose): a redeploy
tears the branch down completely first. A failed deploy therefore leaves the
branch **down, not stale** — the honest cost of full-replace, acceptable for
test environments. A failed deploy never affects another branch: the blast
radius is exactly one branch's network and containers.

## Control API

| method + path | body / auth | effect |
| --- | --- | --- |
| `POST /deploy` | deploy JSON, bearer | validate, start a full-replace deploy, return 202 + URLs, or 400/401 |
| `DELETE /deploy/{branch}` | bearer | remove the branch's containers + network, drop its routes; idempotent (204 even if absent) |
| `GET /deployments` | bearer | list branches with status, services, and URLs, rebuilt from Docker labels |
| `GET /healthz` | none | liveness for the control API itself |

## Startup reconciliation

On boot, hoster lists containers labelled `hoster.branch`, groups them by
branch, and rebuilds the routing table from `hoster.hostname` + `hoster.port` +
the container's inspected IP. Containers survive a hoster restart, and routing
is restored with no redeploy. Containers whose network or IP can no longer be
resolved are logged and skipped, not routed.

## Error handling

| failure | behaviour |
| --- | --- |
| bad token | 401, nothing created |
| invalid config | 400, nothing created |
| image pull fails | deploy marked `failed`, error surfaced in `GET /deployments`, branch left down |
| container exits immediately | readiness gate times out → `failed`, containers left for inspection |
| readiness never passes | `failed` after timeout |
| Docker daemon unreachable | 503 on `POST /deploy`; the proxy keeps serving existing routes from the in-memory table |
| hoster restarts mid-deploy | partial containers are reconciled on boot; an incomplete branch is torn down on next deploy (full replace) |

The proxy and its in-memory routing table are never blocked by Docker being
slow or down; only new deploys are.

## Testing

The `ContainerRuntime` seam makes almost everything testable without Docker.

- **config / template / labels** — pure functions, table-driven unit tests
  (validation rejections, substitution, `{{url}}` resolution, label↔route
  mapping).
- **engine** — orchestration against an in-memory `FakeRuntime`: full-replace
  teardown ordering, exposed-vs-internal routing, `{{url}}` wiring, readiness
  gating (injected clock/checker), routing-table swap, one-branch blast radius.
- **api** — real HTTP against the engine backed by `FakeRuntime`: auth 401,
  validation 400, 202 happy path, `DELETE` idempotency, `GET /deployments`.
- **docker.rs** — integration tests against a live socket, **skipped when no
  socket is present** (a runtime probe, not a hard dependency), so the suite is
  green on a machine without Docker and thorough on one with it. Covers: network
  create/remove, run+inspect returns a reachable IP, service-name DNS resolves
  between two containers, label listing, full-replace removes prior containers.

## Operator setup

Environment / config keys (via `settings.rs`):

- `HOSTER_LISTEN` — public proxy addr (default `127.0.0.1:8080`, as today).
- `HOSTER_API_LISTEN` — control API addr (default `127.0.0.1:8081`).
- `HOSTER_HOSTNAME_TEMPLATE` — e.g. `{service}-{branch}.dev.example.com`.
  Validated at startup to produce legal DNS labels.
- `HOSTER_REGISTRY` — registry base for `{{registry}}`.
- `HOSTER_TOKEN` — shared bearer token for the control API.
- `DOCKER_HOST` — standard Docker socket selection; `bollard` honours it via
  `connect_with_local_defaults`.

## Build order

Each step is independently useful and testable; the plan follows this order.

1. `hoster.json` config model + validation.
2. Template substitution.
3. `ContainerRuntime` trait + types + `FakeRuntime`.
4. Labels + reconciliation mapping.
5. Deploy engine (full-replace orchestration) over the trait.
6. `DockerRuntime` over bollard (live-socket integration tests).
7. Control API + shared-token auth.
8. Wire `main` (proxy + API + engine + startup reconcile), delete the routes
   file scaffolding, operator settings, end-to-end.
