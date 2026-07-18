# Deploying a project with hoster

hoster gives every git branch its own live environment. Your CI pushes an image
and makes one HTTP call; a few seconds later `backend-<branch>.<your-domain>`
serves that branch. This guide is everything you need to run hoster and point
your own project at it.

> **Status of this build.** hoster currently serves **plain HTTP** (no TLS yet)
> and authenticates the control API with **one shared token**. It is built for
> internal testing environments, not public production. TLS, per-project
> tokens, TTL expiry, and a dashboard are planned but not in this build. See
> [Limitations](#limitations).

---

## How it works in one picture

```
   CI ──POST /deploy {branch, tag, config}──▶  hoster control API  (:8081)
                                                      │
                                                      ▼
                                          Docker: per-branch network
                                          pull + run each service
                                          learn container IPs
                                                      │  swaps routing table
                                                      ▼
   browser ──https? no, http──▶  hoster proxy (:8080) ──Host header──▶ container
      backend-mybranch.dev.example.com  →  10.x.x.x:8080
```

- Each branch gets its **own Docker network**. Containers on it reach each other
  by **service name** — `postgres:5432` resolves with no configuration.
- Only services you mark `expose` get a public hostname. Everything else
  (databases, caches) is reachable only from inside the branch.
- The proxy routes public requests by the `Host` header, so every branch shares
  one IP and one port. No per-branch port juggling.

---

## Prerequisites

1. **A Linux host with Docker** (or any Docker-API-compatible daemon: Docker
   Engine, Podman with the Docker socket, OrbStack/Colima for local dev).
   hoster must run **on that host**, next to the daemon — it talks to the Docker
   socket and dials container IPs directly.
2. **Wildcard DNS** pointing at the host. If your hostname template is
   `{service}-{branch}.dev.example.com`, create an `A` record for
   `*.dev.example.com` → the host's IP.
3. **A container registry** your CI pushes images to and the host can pull from.
4. **The `hoster` binary** — `cargo build --release`, then run
   `target/release/hoster`.

---

## Running hoster

hoster is configured entirely through environment variables. `HOSTER_TOKEN` is
the only required one.

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOSTER_TOKEN` | *(required)* | Shared bearer token CI must send to the control API. Choose a long random string. |
| `HOSTER_LISTEN` | `127.0.0.1:8080` | Address the **public proxy** binds. Set to `0.0.0.0:80` to accept traffic from outside the host. |
| `HOSTER_API_LISTEN` | `127.0.0.1:8081` | Address the **control API** binds. Keep this private (localhost or a VPN interface) — anyone who can reach it and holds the token can deploy. |
| `HOSTER_HOSTNAME_TEMPLATE` | `{service}-{branch}.dev.example.com` | How public hostnames are built. `{service}` and `{branch}` are substituted. |
| `HOSTER_REGISTRY` | `localhost:5000` | Registry base used for the `{{registry}}` template variable in image refs. |
| `DOCKER_HOST` | *(standard Docker default)* | Socket selection, honoured by the Docker client. Set it if your socket is non-standard (e.g. OrbStack/Colima). |

Example:

```bash
export HOSTER_TOKEN='paste-a-long-random-secret-here'
export HOSTER_LISTEN='0.0.0.0:80'
export HOSTER_API_LISTEN='127.0.0.1:8081'
export HOSTER_HOSTNAME_TEMPLATE='{service}-{branch}.dev.example.com'
export HOSTER_REGISTRY='registry.example.com'
./hoster
```

On startup hoster reconciles its routing table from any hoster-labelled
containers already running, so a restart restores routing without redeploying.
If Docker is unreachable at startup, hoster logs a warning and still serves the
proxy — deploys fail until the daemon returns, but existing routes keep working.

---

## Describing your project: `hoster.json`

Each branch ships a `hoster.json` in its repo describing its services. Your CI
sends the file's contents in the deploy request; hoster never clones your repo.

```json
{
  "project": "myapp",
  "services": {
    "postgres": {
      "image": "postgres:16",
      "env": { "POSTGRES_PASSWORD": "dev", "POSTGRES_DB": "app" }
    },
    "backend": {
      "image": "{{registry}}/myapp-backend:{{tag}}",
      "env": {
        "DATABASE_URL": "postgres://postgres:dev@postgres:5432/app",
        "PUBLIC_URL": "{{url.backend}}"
      },
      "expose": { "port": 8080, "health": "/healthz" }
    },
    "frontend": {
      "image": "{{registry}}/myapp-frontend:{{tag}}",
      "env": { "API_URL": "{{url.backend}}" },
      "expose": { "port": 3000 }
    }
  }
}
```

### Fields

**Top level**

| field | required | meaning |
| --- | --- | --- |
| `project` | yes | A name for your project. |
| `services` | yes | Map of service name → service. At least one. |
| `ttl` | no | Accepted but currently ignored (no expiry yet). Safe to include. |

Service names must be DNS labels: lowercase letters, digits, and hyphens.

**A service**

| field | required | meaning |
| --- | --- | --- |
| `image` | yes | Image reference. May use template variables. |
| `env` | no | Environment variables passed to the container. Values may use template variables. |
| `expose` | no | **Absent = internal-only.** The service runs and is reachable by name from other containers, but gets no public hostname. |

**`expose`**

| field | required | meaning |
| --- | --- | --- |
| `port` | yes | The port your service listens on **inside the container**. Not a host port. |
| `subdomain` | no | Use this name instead of the service name in the public hostname (e.g. `api` publicly, `backend` internally). |
| `health` | no | HTTP path hoster polls until it answers (any status below 500) before routing traffic. Without it, hoster waits for the port to accept a TCP connection. |

### How services talk to each other

Inside a branch, use **service names** — they resolve automatically:

```
DATABASE_URL = postgres://postgres:dev@postgres:5432/app
```

This string is identical on every branch. Don't put IPs or public URLs in
container-to-container config.

Use a **public URL only when the browser needs it** — e.g. a frontend calling
the backend from the user's browser. That's what `{{url.backend}}` is for.

### Template variables

Substituted at deploy time in `image` and `env` values:

| variable | becomes |
| --- | --- |
| `{{registry}}` | your `HOSTER_REGISTRY` |
| `{{tag}}` | the image tag from the deploy request |
| `{{branch}}` | the sanitized branch name |
| `{{sha}}` | the commit sha from the deploy request |
| `{{url.<service>}}` | the full public URL of an exposed service, e.g. `http://backend-mybranch.dev.example.com` |

`{{url.<service>}}` only works for services that have an `expose` block —
referencing an internal service is a validation error.

---

## Project environment & secrets

Keep secrets — API keys, tokens — **out** of `hoster.json` and the image. Store
them in hoster instead, per project, and hoster injects them into that project's
services on every deploy. Set them in the dashboard's **Environment** section
(grouped under your project) or via the control API:

```bash
# set/replace a variable; "services":[] targets every service
curl -fsS -X PUT "http://hoster.internal:8081/projects/myapp/vars/GOOGLE_API_KEY" \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"value":"AIza…","services":["backend"]}'
```

- The `project` in the path must match `project` in your `hoster.json`.
- **Precedence:** on a key conflict the stored value wins over `hoster.json`.
  Stored values are injected verbatim — no `{{…}}` templating.
- **Masked:** `GET /projects` and the dashboard show keys and target services,
  never values.
- The injected value is present in the container's environment (visible via
  `docker inspect` on the host); it never appears in labels or logs.

---

## The control API

All requests except `GET /healthz` require the header
`Authorization: Bearer <HOSTER_TOKEN>`.

### `POST /deploy` — create or replace a branch

```json
{
  "branch": "feature/JIRA-123",
  "tag": "a1b2c3d",
  "sha": "a1b2c3d4e5f6...",
  "config": { "...contents of hoster.json..." }
}
```

Returns **202 Accepted** with the branch's public URLs as soon as the request is
validated; the containers come up in the background.

```json
{ "branch": "feature-jira-123",
  "urls": { "backend": "http://backend-feature-jira-123.dev.example.com",
            "frontend": "http://frontend-feature-jira-123.dev.example.com" } }
```

Note the branch name is **sanitized** into a DNS label: `feature/JIRA-123`
becomes `feature-jira-123`. Use the sanitized form (or the returned URLs) when
referring to the branch.

Deploying is **full replace**: an existing deployment for the same branch is torn
down first — containers and volumes included — then recreated. Data does not
survive a redeploy, so seed databases from an init step in your containers.

### `DELETE /deploy/{branch}` — tear a branch down

```
DELETE /deploy/feature-jira-123
```

Returns **204**. Idempotent — deleting a branch that isn't there still returns
204. Use the sanitized branch name.

### `GET /deployments` — list what's deployed

Returns a JSON array of `{ branch, status, urls }`. `status` is `provisioning`,
`running`, or `failed: <reason>`.

### `GET /healthz` — liveness

Returns `200 ok`, no auth. For monitoring the control API itself.

---

## Wiring it into CI

After your CI builds and pushes the branch's image, add one step that calls
hoster. Example (GitHub Actions, but any CI works — it's just an HTTP call):

```yaml
- name: Deploy branch to hoster
  run: |
    curl -fsS -X POST "http://hoster.internal:8081/deploy" \
      -H "Authorization: Bearer ${{ secrets.HOSTER_TOKEN }}" \
      -H "Content-Type: application/json" \
      -d "$(jq -n \
            --arg branch "${{ github.ref_name }}" \
            --arg tag    "${{ github.sha }}" \
            --arg sha    "${{ github.sha }}" \
            --argjson config "$(cat hoster.json)" \
            '{branch:$branch, tag:$tag, sha:$sha, config:$config}')"
```

Point CI at your `HOSTER_API_LISTEN` address (over your VPN / internal network —
it should not be publicly reachable). Store `HOSTER_TOKEN` as a CI secret.

The image `tag` you send must match what `{{tag}}` produces in your
`hoster.json` image refs — using the commit sha for both is the simplest choice.

---

## A full walkthrough

Branch `feature/checkout` of project `myapp`, using the `hoster.json` above and
template `{service}-{branch}.dev.example.com`:

1. CI builds and pushes `registry.example.com/myapp-backend:<sha>` and
   `...frontend:<sha>`.
2. CI calls `POST /deploy` with `branch=feature/checkout`, `tag=<sha>`, and the
   `hoster.json`.
3. hoster sanitizes the branch to `feature-checkout`, creates network
   `hoster-feature-checkout`, and starts `postgres`, `backend`, `frontend` on it.
4. `backend` reaches Postgres at `postgres:5432`. `frontend` is handed
   `API_URL=http://backend-feature-checkout.dev.example.com` for the browser.
5. hoster waits for `backend`'s `/healthz` and `frontend`'s port, then routes:
   - `http://backend-feature-checkout.dev.example.com` → backend container
   - `http://frontend-feature-checkout.dev.example.com` → frontend container
   - Postgres has no public URL.
6. You open the frontend URL and test the branch. Every push repeats the cycle
   from a clean slate.
7. When done: `DELETE /deploy/feature-checkout`.

---

## Verifying your setup

With Docker running on the host:

```bash
# 1. hoster answers
curl -s http://127.0.0.1:8081/healthz            # -> ok

# 2. deploy a trivial branch
curl -fsS -X POST http://127.0.0.1:8081/deploy \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"branch":"demo","tag":"latest","sha":"x","config":{
        "project":"p","services":{
          "web":{"image":"nginx:alpine","expose":{"port":80}}}}}'

# 3. hit it through the proxy (Host header selects the branch)
curl -s -H 'Host: web-demo.dev.example.com' http://127.0.0.1:8080/ | head -1

# 4. list, then tear down
curl -s http://127.0.0.1:8081/deployments -H "Authorization: Bearer $HOSTER_TOKEN"
curl -s -X DELETE http://127.0.0.1:8081/deploy/demo -H "Authorization: Bearer $HOSTER_TOKEN"
```

If step 3 returns nginx's welcome page, routing works end to end.

---

## Limitations

Known and deliberate for this build — plan around them:

- **Plain HTTP, no TLS.** Branch URLs are `http://`. Run hoster behind a
  TLS-terminating proxy if you need HTTPS today; native TLS is a planned
  milestone.
- **One shared token.** Every CI caller uses the same `HOSTER_TOKEN`. Keep the
  control API off the public internet. Per-project tokens are planned.
- **HTTP services only.** Public routing works by the HTTP `Host` header. Raw
  TCP services (a database you want to reach from your laptop) cannot be
  exposed publicly — use an SSH tunnel to the host and dial the container.
- **Ephemeral.** Every deploy is a full replace; volumes do not survive. Seed
  data from your container's startup, not once by hand.
- **No automatic expiry.** `ttl` is accepted but not enforced yet — tear down
  branches with `DELETE` (e.g. from a CI job when a branch merges or closes).
- **Single host.** hoster runs one machine's worth of branches. No multi-node
  scheduling.

---

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| `POST /deploy` returns 401 | Missing or wrong `Authorization: Bearer` token. |
| `POST /deploy` returns 400 | Invalid `hoster.json` (unknown field, empty services, bad service name, zero port, `{{url.x}}` to a non-exposed service). |
| Branch URL returns 404 | Wrong `Host` header, deploy still provisioning, or the service has no `expose` block. Check `GET /deployments`. |
| Deploy status `failed` | Image pull failed, container exited immediately, or it never became ready (health/port check timed out after 30s). |
| Everything 404s from outside the host | `HOSTER_LISTEN` is `127.0.0.1` (localhost only). Bind `0.0.0.0:<port>` for external traffic, and confirm wildcard DNS points at the host. |
