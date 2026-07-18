# hoster

hoster gives every git branch its own live environment. Your CI pushes an image
and makes one HTTP call; a few seconds later `backend-<branch>.<your-domain>`
serves that branch. It runs on a single Docker host, routes public traffic by
the HTTP `Host` header, and keeps each branch on its own Docker network.

```
   CI ──POST /deploy {branch, tag, config}──▶  hoster control API  (:8081)
                                                      │
                                                      ▼
                                          Docker: per-branch network
                                          pull + run each service
                                          learn container IPs
                                                      │  swaps routing table
                                                      ▼
   browser ──http──▶  hoster proxy (:80) ──Host header──▶ container
      backend-mybranch.dev.example.com  →  10.x.x.x:8080
```

> **Status.** hoster serves **plain HTTP** (no built-in TLS) and authenticates
> the control API with **one shared token**. It targets internal testing
> environments, not public production. Front it with a TLS terminator for
> HTTPS. See [Limitations](#limitations).

## Contents

- [How it works](#how-it-works)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
  - [Quick install (recommended)](#quick-install-recommended)
  - [What the installer does](#what-the-installer-does)
  - [Upgrading](#upgrading)
  - [Uninstalling](#uninstalling)
  - [Install from source](#install-from-source)
- [Configuration](#configuration)
  - [Environment variables](#environment-variables)
  - [The config file and the service](#the-config-file-and-the-service)
  - [Ports and binding](#ports-and-binding)
  - [Wildcard DNS](#wildcard-dns)
  - [HTTPS with a reverse proxy](#https-with-a-reverse-proxy)
  - [The dashboard](#the-dashboard)
- [Verifying your setup](#verifying-your-setup)
- [Deploying a project](#deploying-a-project)
- [Releasing (maintainers)](#releasing-maintainers)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)

---

## How it works

- Each branch gets its **own Docker network**. Containers on it reach each other
  by **service name** — `postgres:5432` resolves with no configuration.
- Only services you mark `expose` get a public hostname. Everything else
  (databases, caches) is reachable only from inside the branch.
- The proxy routes public requests by the `Host` header, so every branch shares
  one IP and one port. No per-branch port juggling.
- On startup hoster reconciles its routing table from any hoster-labelled
  containers already running, so a restart restores routing without
  redeploying. If Docker is unreachable at startup, hoster logs a warning and
  still serves the proxy — deploys fail until the daemon returns, but existing
  routes keep working.

---

## Prerequisites

1. **A Linux host with Docker and systemd.** Any Docker-API-compatible daemon
   works (Docker Engine, Podman with the Docker socket). hoster must run **on
   that host**, next to the daemon — it talks to the Docker socket and dials
   container IPs directly.
2. **Wildcard DNS** pointing at the host (see [Wildcard DNS](#wildcard-dns)).
3. **A container registry** your CI pushes images to and the host can pull from.
4. **`curl` or `wget`** on the host (the installer uses whichever is present).

---

## Installation

### Quick install (recommended)

On the host, one command downloads the release binary, installs a hardened
systemd service and a config file, and starts it:

```bash
curl -fsSL https://raw.githubusercontent.com/smartline-ai/hoster/main/scripts/install.sh | sudo sh
```

Pin a specific version instead of the latest release:

```bash
curl -fsSL https://raw.githubusercontent.com/smartline-ai/hoster/main/scripts/install.sh | sudo VERSION=v0.1.0 sh
```

The installer downloads the static `x86_64-linux-musl` binary and **verifies its
SHA-256 checksum** before installing — it runs on any x86_64 Linux regardless of
glibc version.

### What the installer does

- Installs the binary to `/usr/local/bin/hoster` (atomic replace).
- Creates a `hoster` system user (no login) and adds it to the `docker` group so
  the service can reach the Docker socket.
- Writes `/etc/hoster/hoster.env` **only if it does not already exist** — it
  never overwrites your token. A random `HOSTER_TOKEN` is generated if you don't
  supply one.
- Installs `/etc/systemd/system/hoster.service` — runs as the `hoster` user with
  `NoNewPrivileges`, `ProtectSystem=full`, `ProtectHome`, `PrivateTmp`, and
  `CAP_NET_BIND_SERVICE` so the non-root service may bind port 80.
- Runs `systemctl enable --now hoster` and prints next steps.

You can override defaults with environment variables on the install command:
`VERSION`, `HOSTER_REPO`, `PREFIX`, `HOSTER_TOKEN`, `HOSTER_LISTEN`,
`HOSTER_API_LISTEN`, `HOSTER_HOSTNAME_TEMPLATE`, `HOSTER_REGISTRY`.

### Upgrading

Re-run the same install command. It replaces the binary and restarts the
service; `/etc/hoster/hoster.env` is left untouched.

### Uninstalling

```bash
sudo sh install.sh --uninstall   # stop + remove service and binary; keep config
sudo sh install.sh --purge       # also remove /etc/hoster and the hoster user
```

### Install from source

Requires a recent Rust toolchain (edition 2024).

```bash
git clone https://github.com/smartline-ai/hoster
cd hoster
cargo build --release
sudo install -m 0755 target/release/hoster /usr/local/bin/hoster
```

For a portable static binary identical to the release artifact:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

Then run it directly with the environment variables below, or wire up your own
systemd unit modelled on [`scripts/install.sh`](scripts/install.sh).

---

## Configuration

hoster is configured entirely through environment variables. `HOSTER_TOKEN` is
the only required one. The installer keeps them in `/etc/hoster/hoster.env`.

### Environment variables

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOSTER_TOKEN` | *(required)* | Shared bearer token CI must send to the control API. Choose a long random string. |
| `HOSTER_LISTEN` | `127.0.0.1:8080` | Address the **public proxy** binds. The installer sets it to `0.0.0.0:80`. Use `0.0.0.0:<port>` to accept outside traffic. |
| `HOSTER_API_LISTEN` | `127.0.0.1:8081` | Address the **control API** (and dashboard) binds. Keep it private — anyone who can reach it and holds the token can deploy. |
| `HOSTER_HOSTNAME_TEMPLATE` | `{service}-{branch}.dev.example.com` | How public hostnames are built. `{service}` and `{branch}` are substituted. |
| `HOSTER_REGISTRY` | `localhost:5000` | Registry base used for the `{{registry}}` template variable in image refs. |
| `HOSTER_DASHBOARD_PASSWORD` | *(unset)* | Set a non-empty value to enable the web [dashboard](#the-dashboard). Unset disables it. |
| `DOCKER_HOST` | *(Docker default)* | Socket selection, honoured by the Docker client. Set it if your socket is non-standard. |
| `RUST_LOG` | `hoster=info` | Log filter (`tracing`/`env_filter` syntax), e.g. `hoster=debug`. |

Example `/etc/hoster/hoster.env`:

```bash
HOSTER_TOKEN=paste-a-long-random-secret-here
HOSTER_LISTEN=0.0.0.0:80
HOSTER_API_LISTEN=127.0.0.1:8081
HOSTER_HOSTNAME_TEMPLATE={service}-{branch}.dev.example.com
HOSTER_REGISTRY=registry.example.com
HOSTER_DASHBOARD_PASSWORD=another-long-secret
```

### The config file and the service

```bash
sudo systemctl status hoster        # is it up?
sudo journalctl -u hoster -f        # follow logs
sudoedit /etc/hoster/hoster.env     # edit configuration
sudo systemctl restart hoster       # apply changes (config is read at startup)
sudo systemctl enable hoster        # start on boot (installer does this already)
```

The service depends on `docker.service` (soft dependency) and restarts
automatically on failure.

### Ports and binding

- **Proxy** (`HOSTER_LISTEN`): public HTTP traffic for every branch. Bind
  `0.0.0.0:80` (the installer default) to accept traffic from outside the host.
  Binding a port below 1024 as the non-root service works because the unit
  grants `CAP_NET_BIND_SERVICE`. If you put a reverse proxy in front, bind the
  proxy to `127.0.0.1:8080` instead and let the reverse proxy own `:80`/`:443`.
- **Control API** (`HOSTER_API_LISTEN`): the deploy API and dashboard. Keep it
  on `127.0.0.1` or a private/VPN interface. It must **not** be publicly
  reachable — the shared token is the only thing protecting it.

### Wildcard DNS

Public hostnames all share one IP, so a single wildcard record covers every
branch. If your template is `{service}-{branch}.dev.example.com`, create:

```
*.dev.example.com   A   <the host's public IP>
```

Set `HOSTER_HOSTNAME_TEMPLATE` to match the domain you chose.

### HTTPS with a reverse proxy

hoster speaks plain HTTP. For HTTPS, terminate TLS in front of it and forward
requests, preserving the `Host` header (hoster routes on it). Point the proxy
listener at localhost and let the terminator own the public ports:

```
HOSTER_LISTEN=127.0.0.1:8080
```

Example nginx server block for the wildcard (needs a wildcard TLS certificate,
e.g. from a DNS-01 ACME challenge):

```nginx
server {
    listen 443 ssl;
    server_name *.dev.example.com;

    ssl_certificate     /etc/letsencrypt/live/dev.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/dev.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;                      # hoster routes on this
        proxy_set_header X-Forwarded-For $remote_addr;
        proxy_set_header X-Forwarded-Proto https;
    }
}
```

Redirect `:80` to `:443` in a separate server block if you want to force HTTPS.

### The dashboard

Setting `HOSTER_DASHBOARD_PASSWORD` enables a small server-rendered web
dashboard that lists deployments and lets you tear a branch down. It is served
on the **control API listener** (`HOSTER_API_LISTEN`) at:

- `GET /login`, `POST /login` — password login (sets a session cookie)
- `GET /` — the dashboard (lists branches, status, and URLs)
- `POST /ui/destroy/<branch>` — tear a branch down from the UI
- `POST /logout`

Because it lives on the private API listener, reach it the same way you reach
the control API: over your VPN/private interface, an SSH tunnel, or behind a
separate TLS-terminating, access-controlled vhost. Leave
`HOSTER_DASHBOARD_PASSWORD` unset to disable the dashboard entirely (its routes
then return `503 dashboard not configured`).

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

## Deploying a project

Each branch ships a `hoster.json` in its repo describing its services. Your CI
builds and pushes images, then sends the file's contents to hoster in one HTTP
call — hoster never clones your repo.

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
    }
  }
}
```

Services reach each other by **name** (`postgres:5432`) on the branch's private
network; only services with an `expose` block get a public hostname. Template
variables substituted at deploy time: `{{registry}}`, `{{tag}}`, `{{branch}}`,
`{{sha}}`, and `{{url.<service>}}` (the public URL of an exposed service).

### Control API

All requests except `GET /healthz` require
`Authorization: Bearer <HOSTER_TOKEN>`.

| Method + path | Purpose |
| --- | --- |
| `POST /deploy` | Create or replace a branch. Body: `{branch, tag, sha, config}`. Returns **202** with the branch's public URLs; containers come up in the background. |
| `DELETE /deploy/{branch}` | Tear a branch down. Returns **204**; idempotent. Use the sanitized branch name. |
| `GET /deployments` | List `{branch, status, urls}`. `status` is `provisioning`, `running`, or `failed: <reason>`. |
| `GET /healthz` | Liveness. Returns `200 ok`, no auth. |

Branch names are **sanitized** to DNS labels (`feature/JIRA-123` →
`feature-jira-123`). Deploying is **full replace**: the existing deployment for
a branch is torn down (containers and volumes) and recreated, so data does not
survive a redeploy — seed databases from a container init step.

Wire it into CI after your image build/push:

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

The full `hoster.json` field reference and a step-by-step walkthrough are in
**[docs/deploying.md](docs/deploying.md)**.

---

## Releasing (maintainers)

Releases are cut from git tags. Bump `version` in `Cargo.toml`, then:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The [`release`](.github/workflows/release.yml) workflow verifies the tag matches
`Cargo.toml`, builds the `x86_64-unknown-linux-musl` binary, and publishes a
GitHub Release with the binary, a `.tar.gz` bundle (binary + installer), and
`SHA256SUMS`. The [`ci`](.github/workflows/ci.yml) workflow runs fmt, clippy,
build, tests, and shellcheck on every pull request and push to `main`.

---

## Limitations

Known and deliberate for this build — plan around them:

- **Plain HTTP, no built-in TLS.** Branch URLs are `http://`. Run hoster behind
  a TLS terminator for HTTPS.
- **One shared token.** Every CI caller uses the same `HOSTER_TOKEN`. Keep the
  control API off the public internet.
- **HTTP services only.** Public routing works by the HTTP `Host` header. Raw
  TCP services cannot be exposed publicly — SSH-tunnel to the host and dial the
  container instead.
- **Ephemeral.** Every deploy is a full replace; volumes do not survive. Seed
  data from your container's startup.
- **No automatic expiry.** `ttl` is accepted but not enforced yet — tear down
  branches with `DELETE` (e.g. from CI when a branch merges or closes).
- **Single host.** hoster runs one machine's worth of branches. No multi-node
  scheduling.

---

## Troubleshooting

| Symptom | Likely cause |
| --- | --- |
| Service won't start; `journalctl -u hoster` shows `HOSTER_TOKEN must be set` | `HOSTER_TOKEN` empty in `/etc/hoster/hoster.env`. |
| Service won't start; bind error on `:80` | Another process owns the port, or the unit lacks `CAP_NET_BIND_SERVICE` (re-run the installer). |
| `POST /deploy` returns 401 | Missing or wrong `Authorization: Bearer` token. |
| `POST /deploy` returns 400 | Invalid `hoster.json` (unknown field, empty services, bad service name, zero port, `{{url.x}}` to a non-exposed service). |
| Branch URL returns 404 | Wrong `Host` header, deploy still provisioning, or the service has no `expose` block. Check `GET /deployments`. |
| Deploy status `failed` | Image pull failed, container exited immediately, or it never became ready (health/port check timed out). |
| Everything 404s from outside the host | `HOSTER_LISTEN` is `127.0.0.1` (localhost only). Bind `0.0.0.0:<port>` and confirm wildcard DNS points at the host. |
| Deploys fail with a Docker error | Docker not running, or the `hoster` user isn't in the `docker` group: `sudo usermod -aG docker hoster && sudo systemctl restart hoster`. |
| Dashboard returns `503 dashboard not configured` | `HOSTER_DASHBOARD_PASSWORD` is unset or empty. |
