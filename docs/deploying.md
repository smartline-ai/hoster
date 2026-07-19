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
   `*.dev.example.com` → the host's IP. See [DNS providers](#dns-providers) if
   you'd rather hoster keep that record (and TLS certificates) in sync itself.
3. **A container registry** your CI pushes images to and the host can pull from.
4. **The `hoster` binary** — `cargo build --release`, then run
   `target/release/hoster`.

---

## DNS providers

hoster can manage its own wildcard `A` record — and, with an ACME account
configured, the `_acme-challenge` TXT records DNS-01 issuance needs — instead
of you maintaining the zone by hand. Configure this from the dashboard's
`/settings` page (**TLS & DNS** → **DNS setup**), which offers a guided picker
for four provider kinds:

| Kind | Fields | Notes |
| --- | --- | --- |
| `cloudflare` | `token` | A scoped API token with `Zone:DNS:Edit` on the zone your base domain lives in. |
| `hetzner` | `token` | An API token from the Hetzner DNS console for that zone. |
| `namecheap` | `api_user`, `api_key`, `username` | **Requires an IP allowlist first**: Namecheap rejects API calls from any IP not allowlisted under *API Access* in your account, before it even looks at the credentials — allowlist `HOSTER_PUBLIC_IP` there before saving. |
| `manual` | *(none)* | hoster does not touch DNS. The `/settings` panel still lists the exact records to create by hand — one wildcard `A` record per project base domain, plus the `_acme-challenge` TXT note while TLS is on. |

Any provider except `manual` needs **`HOSTER_PUBLIC_IP`** set to the host's
public IP — it's the target of every wildcard `A` record hoster writes. Leave
it unset only for `manual` mode; the dashboard warns inline if a non-manual
provider is configured without it, since the wildcard record is silently
skipped rather than deploys failing loudly.

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOSTER_PUBLIC_IP` | *(unset)* | The host's public IP, published as every wildcard `A` record's target. Required once any non-manual DNS provider is configured. |

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

## Reverse-proxy mode (standalone vs. nginx)

By default hoster is its own edge proxy: it binds `:80`/`:443` directly and
routes by `Host` header. That's **standalone mode**, and it's unchanged from
the rest of this guide — nothing to configure.

If you'd rather put nginx at the edge — for its TLS stack, existing configs,
or because other sites already share the box — hoster can run in **nginx
mode** instead. nginx terminates TLS and reverse-proxies every request to
hoster's plain HTTP listener (`HOSTER_LISTEN`); hoster keeps issuing and
renewing the wildcard certificates (via the same DNS-01 ACME flow described
elsewhere in this doc) and generates the nginx config that serves them.

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOSTER_PROXY_MODE` | `standalone` | Set to `nginx` to put nginx at the edge instead of hoster. |
| `HOSTER_NGINX_CONF` | `/etc/nginx/conf.d/hoster.conf` | The single config file hoster generates and keeps up to date. |
| `HOSTER_NGINX_RELOAD_CMD` | `systemctl reload nginx` | Command hoster runs after a config it wrote passes `nginx -t`. |

In nginx mode:

- **`HOSTER_HTTPS_LISTEN` is ignored.** nginx owns `:443`; hoster does not
  bind it, no matter what `HOSTER_HTTPS_LISTEN` is set to.
- **`HOSTER_LISTEN` is nginx's upstream.** Point it at a local, plain-HTTP
  address — e.g. `127.0.0.1:8080` — and have nginx proxy to it. Don't expose
  this address publicly; nginx is the public edge now.
- hoster still issues and renews the wildcard TLS certificates itself. It
  just hands them to nginx instead of terminating TLS with them directly.

```bash
export HOSTER_PROXY_MODE='nginx'
export HOSTER_LISTEN='127.0.0.1:8080'
export HOSTER_NGINX_CONF='/etc/nginx/conf.d/hoster.conf'
export HOSTER_NGINX_RELOAD_CMD='systemctl reload nginx'
```

### Lifecycle: generated at startup and on renewal, never per deploy

hoster (re)writes `HOSTER_NGINX_CONF` at **startup** and whenever a
certificate is **issued or renewed** — not on every deploy. A new branch
needs **no nginx change**: the wildcard cert plus hoster's `Host`-based
routing already cover every `*.<base>` subdomain, so nginx keeps working
unmodified as branches come and go.

The generated file has one shared `:80` server block (a catch-all that proxies
plain HTTP to hoster's upstream, same as the `:443` blocks) plus one `:443`
server block per wildcard base domain that currently has a certificate on
disk. A base without a cert yet is simply omitted until issuance catches up.

### Permissions

hoster needs to be able to (a) write `HOSTER_NGINX_CONF` and (b) run
`nginx -t` and the reload command. Either run hoster as root, or grant it a
narrow sudoers entry:

```
hoster ALL=(root) NOPASSWD: /usr/sbin/nginx -t, /bin/systemctl reload nginx
```

Adjust the binary paths to match your distro. If hoster can't write the file
or run these commands, it fails loudly — in the logs and in the dashboard's
Proxy section — rather than silently leaving nginx unconfigured.

### nginx version

The generated config uses `http2 on;` as its own directive, which requires
**nginx ≥ 1.25**. Older nginx needs HTTP/2 enabled on the `listen` line
instead (the deprecated `listen 443 ssl http2;` form) — not supported by the
generator, so upgrade nginx first if you're on an older release.

### Failure behavior

A failed `nginx -t` **never** triggers a reload — hoster restores the
last-good config file, and the edge keeps serving whatever it was already
serving. Check the dashboard's **Settings** page for the read-only **Proxy**
section: it shows the active proxy mode, the generated conf path, and the
result of the last apply attempt, including any `nginx -t` output when it
failed.

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

The scheme in `{{url.<service>}}` — and in every URL hoster reports, including
the deploy response and the dashboard's links — follows whether hoster is
terminating TLS: `https://` when `HOSTER_HTTPS_LISTEN` is set, `http://`
otherwise. A frontend given `{{url.backend}}` therefore never calls its
backend over plain HTTP from an HTTPS page.

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

- **TLS is opt-in.** With `HOSTER_HTTPS_LISTEN` unset there is no HTTPS
  listener and branch URLs are `http://` — run hoster behind a
  TLS-terminating proxy in that case. Set it (see the README's *Built-in
  TLS*) and hoster terminates TLS itself and reports `https://` URLs.
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
