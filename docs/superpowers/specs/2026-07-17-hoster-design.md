# hoster — per-branch deployment environments

**Status:** design approved, pending spec review
**Date:** 2026-07-17

## Goal

One git branch, one deployed environment. CI pushes images to a registry, calls
hoster, and a few seconds later `backend-my-branch.dev.odinvestor.net` serves
that branch. Branches expire on a timer so nobody has to clean up.

hoster is a single Rust binary on a single host. It owns `:80` and `:443`, talks
to containerd over its gRPC socket, and keeps state in SQLite.

## Design constraint

Easy to operate, and boring enough to trust. When a choice comes up between a
clever mechanism and an obvious one, take the obvious one. This constraint is
load-bearing: it is the reason multi-node scheduling, diffed redeploys, and TCP
exposure are all out of scope below, and it should win future arguments too.

## Non-goals

Explicitly not building these. Each is a defensible feature; none is needed to
make the thing useful, and each drags in a subsystem.

- **Multi-host.** Single machine. No scheduling, no placement, no image
  distribution, no distributed state. ~30 concurrent branches on one box is the
  target, and one box handles that comfortably.
- **Diffed redeploys.** Every deploy is a full replace. Diffing config against
  running state is where the bugs live, and full replace on one host takes
  seconds. Clean optimization later; the data model does not block it.
- **Persistent volumes.** Volumes are ephemeral, scoped to one deployment
  generation. See Lifecycle.
- **TCP exposure.** Only HTTP services can be public. See Networking.
- **An SPA dashboard.** Server-rendered HTML. It is a control surface for a
  handful of people.
- **Cloning git.** hoster never has git credentials. CI sends what hoster needs.

## Architecture

Four components in one binary. They are separable because they share almost
nothing.

```
        CI ──POST /deploy──┐
                           ▼
                    ┌─────────────┐        ┌────────────┐
   dashboard ──────▶│   control   │───────▶│   deploy   │──▶ containerd
                    │    plane    │        │   engine   │
                    └─────────────┘        └─────┬──────┘
                           │                     │ swaps
                           ▼                     ▼
                       SQLite            ArcSwap<RoutingTable>
                                                 │ reads
                                          ┌──────▼──────┐
   internet ──:80/:443───────────────────▶│    proxy    │──▶ container IPs
                                          └─────────────┘
```

- **control plane** — HTTP API and dashboard. Auth, validation, persistence.
  Knows nothing about containers.
- **deploy engine** — containerd, network namespaces, certs. Builds routing
  tables. Never sees a request.
- **proxy** — TLS, routing, proxying. Knows nothing about branches; it has a
  map of hostnames to sockets.
- **reaper** — background task, destroys expired deployments.

The proxy and the deploy engine meet at exactly one data structure: an
`ArcSwap<HashMap<String, Route>>`. The engine builds a new map and swaps the
pointer. The proxy reads it per request with no lock. A deploy going live is one
atomic pointer swap; in-flight requests finish against the old map.

This boundary is the main testability win. The proxy can be tested with a map of
fake upstreams and no containerd at all, and the deploy engine can be tested
without ever making an HTTP request.

## Config: `hoster.json`

Lives in the deployed repo, committed alongside the code, versioned with the
branch. A branch that adds Redis adds it to its own config and it exists on that
branch only. Config is reviewable in the PR that changes it.

CI sends the file contents in the deploy request body. hoster never fetches it.

```json
{
  "project": "odinvestor",
  "ttl": "72h",
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
      "expose": { "port": 3000, "auth": "basic" }
    }
  }
}
```

### Service fields

| field | required | meaning |
|---|---|---|
| `image` | yes | Image ref. Template vars allowed. |
| `env` | no | Environment map. Template vars allowed in values. |
| `expose` | no | **Absent means internal-only.** See below. |
| `volumes` | no | Ephemeral scratch mounts. Destroyed on redeploy. |
| `depends_on` | no | Start ordering only. Not a readiness gate. |

### `expose`

Exposure is opt-in and default-closed. A service with no `expose` block gets a
container and an internal DNS name and nothing else — no hostname, no cert, no
route.

Default-closed is a security property, not a style preference. If exposure is
opt-in, a typo yields an unreachable service and you notice in thirty seconds.
If it were opt-out, a typo yields a public database. For the same reason hoster
never infers exposure from the image's `EXPOSE` directive or from port numbers:
both mean "a program listens here", neither means "the internet should reach
here", and the guess would be wrong in the dangerous direction.

| field | required | meaning |
|---|---|---|
| `port` | yes | Port **inside the container**. Never a host port. |
| `subdomain` | no | Public name, if it should differ from the service key. |
| `auth` | no | `"basic"` puts HTTP basic auth in front of this route. |
| `health` | no | Path polled before the route goes live. |

One exposed port per service. Multiple public ports on one container would need
multiple hostnames and names for each; revisit if it bites.

### Template variables

Substituted at deploy time, in `image` and `env` values only.

| var | value |
|---|---|
| `{{registry}}` | Operator-configured registry base. |
| `{{tag}}` | Image tag from the deploy request. |
| `{{branch}}` | Sanitized branch name. |
| `{{sha}}` | Commit SHA from the deploy request. |
| `{{url.<service>}}` | Full public URL of an exposed service, e.g. `https://backend-my-branch.dev.odinvestor.net`. Error at validation if the target is not exposed. |

`{{url.*}}` exists for **browser-facing** values only — the frontend needs
`API_URL` because the user's browser resolves it, not because a container does.
Container-to-container traffic never uses it. See Networking.

## Networking and service discovery

Each branch gets its own network namespace. Inside it, services reach each other
by service name: `postgres:5432`, `backend:8080`. hoster runs a small DNS
resolver per namespace answering service names with container IPs.

The consequence worth internalising: **`DATABASE_URL` is byte-identical on every
branch.** The namespace does the isolating, so there is no per-branch string
rewriting, no re-templating on restart, and no IP in any config.

Every container has its own IP, so ports never collide. `backend` on branch1 is
`10.42.1.5:8080`; `backend` on branch2 is `10.42.2.5:8080`. Both really listen
on 8080. The host binds `:80` and `:443` and nothing else, ever. There is no
port allocator, so there is no port-in-use failure at deploy and no port state
to leak when a deployment dies badly.

Those `10.42.x.x` addresses are reachable from the host only. hoster can dial
into a branch namespace; nothing outside the machine can. That is what keeps
Postgres private.

### Why HTTP only

Public routing works by reading the `Host` header. Raw TCP has no `Host` header,
so there is nothing to route on. Exposing Postgres publicly would require a real
host port per branch, allocated and tracked, plus a public database — a
different subsystem and a bad idea. For `psql` into a branch, use an SSH tunnel
or `nsenter`. This is a permanent scope boundary, not a not-yet.

## Proxy and TLS

Two listeners.

**`:80`** — answers ACME HTTP-01 challenges, 301s everything else to HTTPS.

**`:443`** — rustls terminates TLS, selecting the branch's certificate by SNI
from an in-memory store. Then: read `Host`, look up the route, check basic auth
if configured, stream to the container with hyper. Websocket upgrades pass
through — frontend dev servers need this more than people expect.

If a route exists but its container is not yet healthy, serve a "starting" page
rather than a connection error.

### Certificates

One certificate per branch, with every exposed hostname for that branch as a
SAN. Issued via `instant-acme` in-process during deploy, HTTP-01 challenge.
Cached by branch and reused across redeploys, since redeploys keep the same
hostnames. Renewed by a background task at 30 days.

Design notes worth keeping:

- **In-process, not certbot.** hoster already terminates TLS and owns `:80`, so
  it holds certs in memory and swaps the rustls config on renewal. No subprocess,
  no webroot, no filesystem coordination.
- **HTTP-01, not DNS-01.** Exact hostnames rather than wildcards means no DNS
  provider API is needed at all. This matters concretely: Namecheap gates API
  access behind 20+ domains or $50 of spend plus IP allowlisting.
- **SAN-per-branch, not per-hostname.** Let's Encrypt allows 50 certs/week per
  *registered* domain (`odinvestor.net`, not `dev.odinvestor.net` — the budget is
  shared with everything else on that domain). One cert per branch means only
  *new branches* cost anything; redeploys are free.
- **Issue at deploy, not on first request.** ACME round-trips take seconds. A
  branch goes live only once its cert exists, so no browser ever hangs waiting.
- **Use the LE staging directory during development.** The rate limit is a hard
  week-long lockout.

### Hostname template

Operator-level config, not `hoster.json`:

```
hostname_template = "{service}-{branch}.dev.odinvestor.net"
```

Available: `{service}`, `{branch}`, `{sha}`, `{project}`. Validated at startup
to produce legal DNS labels (63 chars, alphanumeric and hyphen).

**A `{sha}` template will hit the Let's Encrypt rate limit** — every commit
becomes a new hostname needing a new cert, so 50 commits/week locks you out.
This is an external CA constraint that no amount of flexibility on hoster's side
removes. The setting permits it; the docs must warn about it.

## Deploy flow

`POST /deploy` with a per-project bearer token:

```json
{ "branch": "feature/JIRA-123", "tag": "a1b2c3d", "sha": "a1b2c3d...",
  "config": { ...hoster.json contents... } }
```

1. **Authenticate** the token, resolve it to a project.
2. **Validate** the config. Reject unknown fields, bad templates, `{{url.x}}`
   pointing at an unexposed service, illegal hostnames. All validation happens
   here, before anything is created.
3. **Sanitize** the branch name into a DNS label (`feature/JIRA-123` →
   `feature-jira-123`).
4. **Record** the deployment as `provisioning` in SQLite.
5. **Tear down** any existing deployment for this branch — all containers, the
   namespace, the volumes. Full replace.
6. **Create** the namespace and per-branch DNS.
7. **Pull and start** containers in `depends_on` order, substituting templates.
8. **Obtain the cert** if not cached.
9. **Health-gate** exposed services until their `health` path answers.
10. **Swap the routing table.** Mark `running`. Reset the TTL clock.

Returns the branch's public URLs. Idempotent per branch: calling twice with the
same tag produces the same result.

Steps 5–10 run in a background task; the request returns as soon as the
deployment is recorded, so CI is not held open for image pulls. CI polls
`GET /deployments/<branch>` if it wants to wait.

## Lifecycle and TTL

**Full replace on every push.** Every deploy tears down all containers and
volumes and recreates from the new config and tag. A redeploy yields exactly
what a fresh deploy yields, so there is no "works on my branch because of
leftover state" class of bug.

The cost: **seeding happens on every boot.** Branches with a Postgres need an
init container or an entrypoint that runs migrations and fixtures. That is the
app's job, not hoster's, and it must be said clearly in the user-facing docs.

**TTL resets on deploy.** Every push pushes expiry out by the full TTL, so
actively-developed branches never expire and abandoned ones do. Commits are the
signal that someone cares about a branch.

Idle-based TTL (resetting on HTTP requests) was considered and rejected: a
health check, a crawler, or a forgotten browser tab keeps dead branches alive
forever, and then nothing ever reaps.

**Expiry destroys.** Containers, namespace, volumes, routes, cert cache entry.
No state is preserved because there is no state worth preserving. Stopped
containers would just hold disk and confuse the dashboard.

**Pin** is a dashboard toggle exempting a branch from TTL — for the demo next
week. It covers the real use case idle-TTL was reaching for, without the
liveness ambiguity.

The reaper runs every minute, selects expired unpinned deployments, and destroys
them. Destruction is idempotent and safe to retry.

## Auth

Three doors.

**CI** — bearer token on `POST /deploy`, **one token per project**. A compromised
token from one repo cannot touch another project's branches, and rotating one
repo needs no coordination. Tokens are stored hashed and shown once at creation.

**Dashboard** — single shared password from config, session cookie. Thin, but
honest for one internal team. This is a deliberate first step: the dashboard is
public (it is behind the same public TLS as everything else), so it needs *some*
door. OAuth is a clean upgrade later because it only touches the session layer.

**Deployed branches** — public by default, with optional HTTP basic auth per
route via `expose.auth`. Unfinished feature branches on the open internet get
crawled. Since hoster owns the proxy this is cheap now and painful to retrofit.

## State

SQLite via `sqlx`. Single host, no reason for anything larger.

```sql
projects      (id, name, registry_base, created_at)
tokens        (id, project_id, name, hash, created_at, last_used_at)
deployments   (id, project_id, branch, sha, tag, status, config_json,
               pinned, expires_at, created_at, updated_at)
routes        (id, deployment_id, hostname, service, container_ip, port,
               auth_mode)
certs         (id, branch_key, pem, key_pem, expires_at)
```

`deployments.status`: `provisioning | running | failed | destroying`.

The routing table is rebuilt from `routes` at startup, so a hoster restart
restores routing without redeploying anything. Containers survive hoster
restarts; hoster reconciles against containerd on boot and marks orphans.

## Error handling

**A failed deploy leaves the branch down, not stale.** This follows directly from
full-replace: step 5 destroys the old containers, namespace, and volumes before
step 7 creates the new ones, because old and new cannot coexist — they claim the
same service names and volumes in the same namespace. There is no version of
full-replace where a failed deploy rolls back to the previous running state,
because the previous state no longer exists by the time the new one can fail.

This is the honest cost of full-replace, and it is acceptable for test
environments in a way it would not be for production. If it ever stops being
acceptable, the fix is not error handling — it is blue/green deploys, which
means non-ephemeral identity per generation, which is a real redesign.

Within that constraint: a deploy that fails is marked `failed` with the error
recorded, its containers are torn down, and no route is published. A branch's
route goes live only after its containers pass health checks and its cert
exists, so the routing table never points at something that cannot serve.
**Other branches are never affected by a failed deploy** — the blast radius of
any deploy is exactly one branch.

Specific failures:

| failure | behavior |
|---|---|
| Invalid config | 400 before anything is created. |
| Image pull fails | `failed`, error surfaced in dashboard and API. |
| Container exits immediately | `failed` after health-check timeout, logs retained. |
| Health check never passes | `failed` after timeout, containers left for log inspection until reaped. |
| ACME fails | `failed`. Branch does not go live without a cert. |
| containerd unreachable | 503 on deploy; proxy keeps serving existing routes. |
| hoster restarts mid-deploy | `provisioning` rows are reconciled to `failed` on boot. |

## Testing

The component boundaries are what make this tractable.

- **Proxy** — unit tests against a hand-built routing table and a stub upstream.
  No containerd. Covers host routing, unknown host, basic auth, health gating,
  websocket upgrade.
- **Config validation** — pure function, table-driven tests. Every rejection
  case above.
- **Template substitution** — pure function, table-driven.
- **Deploy engine** — integration tests against real containerd in CI. Covers
  full replace, teardown idempotency, namespace isolation, DNS resolution
  between services.
- **Reaper** — unit tests with an injected clock. Never sleep in a test.
- **End-to-end** — one test: deploy a two-service branch, hit its public URL,
  redeploy, hit it again, destroy, confirm 404. Against LE staging.

## Operator setup

1. Wildcard DNS: `*.dev.odinvestor.net` → host IP.
2. **Verify the wildcard resolves at the depth the template needs.** If the
   template nests (`{service}.{branch}.dev...`), confirm with
   `dig +short a.b.dev.odinvestor.net`. RFC 4592 says wildcards match multiple
   labels, but hosted providers implement this inconsistently. The default
   single-label template avoids the question entirely.
3. containerd running, socket accessible.
4. hoster config: domain, registry base, hostname template, dashboard password,
   ACME directory (staging first).
5. Ports 80 and 443 reachable — HTTP-01 needs 80 from the public internet.

## Build order

Each step is independently useful and testable.

1. **Proxy skeleton** — `:443`, static routing table from a config file, hyper
   proxying to a stub. Static self-signed cert.
2. **containerd integration** — start and stop a container, no networking.
3. **Namespaces and DNS** — per-branch netns, service-name resolution, container
   IPs.
4. **Deploy flow** — config parsing, validation, templating, full replace,
   routing table swap. Now genuinely usable over plain HTTP.
5. **ACME** — `instant-acme`, HTTP-01, SAN-per-branch, hot cert swap.
6. **Control plane** — SQLite, tokens, `POST /deploy`, status API.
7. **TTL and reaper** — expiry, pin.
8. **Dashboard** — server-rendered list, logs, destroy, pin.
9. **Basic auth** — per-route.
