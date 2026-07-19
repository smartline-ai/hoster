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

> **Status.** hoster authenticates the control API with **one shared token**
> and targets internal testing environments, not public production. TLS is
> supported and opt-in: set `HOSTER_HTTPS_LISTEN` to have hoster obtain and
> terminate its own Let's Encrypt certificates, or front it with your own TLS
> terminator instead. See [Built-in TLS](#built-in-tls) and
> [Limitations](#limitations).

## Contents

- [How it works](#how-it-works)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
  - [Quick install (recommended)](#quick-install-recommended)
  - [What the installer does](#what-the-installer-does)
  - [Upgrading](#upgrading)
  - [Uninstalling](#uninstalling)
  - [Install from source](#install-from-source)
- [Setting up a host, end to end](#setting-up-a-host-end-to-end)
- [Configuration](#configuration)
  - [Environment variables](#environment-variables)
  - [The config file and the service](#the-config-file-and-the-service)
  - [Ports and binding](#ports-and-binding)
  - [Wildcard DNS](#wildcard-dns)
  - [HTTPS with a reverse proxy](#https-with-a-reverse-proxy)
  - [The dashboard](#the-dashboard)
  - [Project environment & secrets](#project-environment--secrets)
  - [Private registry credentials](#private-registry-credentials)
  - [Per-project domains](#per-project-domains)
  - [Built-in TLS](#built-in-tls)
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
2. **A domain you control**, with **wildcard DNS** pointing at the host (see
   [Wildcard DNS](#wildcard-dns)).
3. **A container registry** your CI pushes images to and the host can pull from.
4. **`curl` or `wget`** on the host (the installer uses whichever is present).
5. **For built-in TLS only: the domain's DNS zone hosted at Cloudflare.**
   Certificates are issued over the ACME DNS-01 challenge, and **Cloudflare is
   the only DNS provider implemented** — hoster needs an API token that can
   write TXT records in the zone. If your DNS lives elsewhere, terminate TLS in
   front of hoster instead (see
   [HTTPS with a reverse proxy](#https-with-a-reverse-proxy)).

New host? Follow [Setting up a host, end to end](#setting-up-a-host-end-to-end)
rather than assembling the reference sections yourself — the order matters.

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

## Setting up a host, end to end

The reference sections below document each knob on its own. This is the order
to turn them in, going from a fresh Linux host to branch environments served
over HTTPS. **The order matters for TLS**: Let's Encrypt counts failed
validations, and a misconfiguration discovered against production instead of
staging can lock you out for a week. Prove each layer works before adding the
next one.

### 1. Check the prerequisites

A Linux host with Docker and systemd, a domain, and — only if you want hoster
to manage its own certificates — that domain's DNS zone at Cloudflare. See
[Prerequisites](#prerequisites) for the full list.

### 2. Install hoster

```bash
curl -fsSL https://raw.githubusercontent.com/smartline-ai/hoster/main/scripts/install.sh | sudo sh
```

This installs the binary, creates a `hoster` system user in the `docker`
group, writes `/etc/hoster/hoster.env` (generating a random `HOSTER_TOKEN` if
you don't pass one), installs `/etc/systemd/system/hoster.service`, and runs
`systemctl enable --now hoster`. Details in
[What the installer does](#what-the-installer-does).

Confirm it came up:

```bash
sudo systemctl status hoster
curl -s http://127.0.0.1:8081/healthz            # -> ok
```

### 3. Configure the essentials

Edit `/etc/hoster/hoster.env`. Four settings matter now:

```bash
HOSTER_TOKEN=paste-a-long-random-secret-here      # the only required variable
HOSTER_LISTEN=0.0.0.0:80                          # public proxy (installer default)
HOSTER_API_LISTEN=127.0.0.1:8081                  # control API + dashboard — keep private
HOSTER_HOSTNAME_TEMPLATE={service}-{branch}.dev.example.com
HOSTER_DASHBOARD_PASSWORD=another-long-secret     # enables the dashboard
```

- `HOSTER_TOKEN` is the **one shared token** every CI caller sends. The
  installer generates one; keep it or replace it with your own long random
  string.
- `HOSTER_HOSTNAME_TEMPLATE` must contain `{branch}`, must include a parent
  domain, and **every placeholder must sit inside the template's first label**.
  That last rule exists because a TLS wildcard certificate matches exactly one
  label: `*.dev.example.com` covers `backend-main.dev.example.com` but nothing
  with a further dot in it. `{service}` is optional, so
  `{branch}.dev.example.com` is fine for a single-service project. Templates
  breaking these rules are rejected.
- `HOSTER_DASHBOARD_PASSWORD` is what enables the dashboard. Leave it unset and
  the dashboard routes return `503 dashboard not configured` — you will want it
  on before the TLS step, because that is where the ACME settings live.
- Ports: keep the control API on `127.0.0.1` or a private interface. See
  [Ports and binding](#ports-and-binding).

Apply the changes — configuration is read at startup only:

```bash
sudo systemctl restart hoster
```

### 4. Point wildcard DNS at the host

One record covers every branch, because every hostname resolves to the same
IP. For the template above:

```
*.dev.example.com   A   <the host's public IP>
```

More in [Wildcard DNS](#wildcard-dns).

### 5. Verify routing over plain HTTP

Do this **before** TLS is in the picture. If routing is broken, you want to
find out now, not while also debugging certificate issuance.

```bash
curl -fsS -X POST http://127.0.0.1:8081/deploy \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"branch":"demo","tag":"latest","sha":"x","config":{
        "project":"p","services":{
          "web":{"image":"nginx:alpine","expose":{"port":80}}}}}'

# from another machine, over real DNS:
curl -s http://web-demo.dev.example.com/ | head -1

curl -s -X DELETE http://127.0.0.1:8081/deploy/demo -H "Authorization: Bearer $HOSTER_TOKEN"
```

nginx's welcome page means DNS, the proxy, and Docker networking all work.
See [Verifying your setup](#verifying-your-setup) for the localhost-only
variant.

### 6. Turn on TLS

Skip this whole step if you terminate TLS with your own reverse proxy — see
[HTTPS with a reverse proxy](#https-with-a-reverse-proxy) instead. TLS is
**entirely opt-in**: with `HOSTER_HTTPS_LISTEN` unset there is no HTTPS
listener, no renewal loop, and no issuance at all, so upgrading an existing
install changes nothing.

**6a. Create a Cloudflare API token.** In the Cloudflare dashboard, scope it to
`Zone:DNS:Edit` on **just the zones hoster serves** — not a global API key.
This token can rewrite DNS for every zone it covers, so the scope is the whole
of your blast radius. hoster only ever creates and deletes `_acme-challenge`
TXT records.

**6b. Enter the ACME settings in the dashboard.** Open `/settings` on the
control API listener and use the **TLS & DNS** panel:

- **ACME account** — your email, and optionally a *control hostname* (a plain,
  non-wildcard name such as `hoster.example.com`). hoster issues a certificate
  for that name and serves the API and dashboard on it over the HTTPS listener.
- **DNS provider** — `cloudflare` and the token from 6a.

The token is written to `HOSTER_PROJECTS_FILE` under mode `0600` and is
**never displayed again**; the dashboard and `GET /acme/status` only report
that a provider is configured. The same fields are available over the
bearer-token API — see [Built-in TLS](#built-in-tls).

**6c. Set `HOSTER_HTTPS_LISTEN` to a high port first.** Start on `:8443` so
hoster's HTTPS listener runs *alongside* whatever already owns `:443` rather
than fighting it for the port:

```bash
sudoedit /etc/hoster/hoster.env      # HOSTER_HTTPS_LISTEN=0.0.0.0:8443
sudo systemctl restart hoster
```

This starts the listener and the renewal loop, which issues a certificate for
every domain hoster wants one for: the global hostname template, every
project's domain override, and the control hostname if set.

**6d. Watch the certificate table — and expect a browser warning.** The
**Certificates** panel on `/settings` lists one row per domain, each with a
state of `pending`, `valid until <date>`, or `failed: <reason>`. `GET
/acme/status` returns the same rows as JSON.

**Staging is the default.** Until you set `HOSTER_ACME_PRODUCTION`, hoster
requests certificates from Let's Encrypt's *staging* environment, and staging
certificates are deliberately **not trusted by browsers**. When a domain reads
`valid until …` and your browser still shows a certificate warning, that is
the flow working, not failing: it proves DNS-01, the Cloudflare token, and the
renewal loop all function before production's rate limits are at stake.

Confirm the certificate is real, warning notwithstanding:

```bash
openssl s_client -connect <host>:8443 -servername backend-main.dev.example.com </dev/null \
  | openssl x509 -noout -text | grep -A1 "Subject Alternative Name"
```

A renewal pass runs every six hours, which is far too slow to test a token you
just pasted. Use **Retry now** in the TLS & DNS panel (or `POST /acme/renew`)
to run a pass immediately and clear the backoff from failures the old
configuration caused. Requests are rate-limited to **one every 20 minutes**;
one that arrives sooner is rejected with `429` and the number of seconds to
wait, rather than silently doing nothing.

Why the caution: Let's Encrypt allows **5 failed validations per identifier
per hour** and **50 certificates per registered domain per week**. Burn those
on production while your token scope is wrong and recovery takes hours to a
week. Staging costs you nothing.

**6e. Switch to production.** Only once every domain you care about reads
`valid` on staging:

```bash
sudoedit /etc/hoster/hoster.env      # HOSTER_ACME_PRODUCTION=1
sudo systemctl restart hoster
```

Accepted values are `1`, `true`, or `yes`. Watch the table again; this time
the browser warning should be gone.

**6f. Move to `:443` and retire the old terminator.**

```bash
sudoedit /etc/hoster/hoster.env      # HOSTER_HTTPS_LISTEN=0.0.0.0:443
sudo systemctl restart hoster
sudo systemctl stop nginx && sudo systemctl disable nginx
```

Binding `:443` as the non-root `hoster` user works because the installed unit
grants `CAP_NET_BIND_SERVICE`.

Throughout, remember the failure mode: **a domain whose certificate could not
be obtained keeps serving plain HTTP rather than going dark.** One bad token
or typoed hostname degrades one domain instead of taking every branch offline
— but it also means nothing crashes to tell you. The **Certificates** panel on
`/settings` is where you see it, so check it after changing DNS credentials or
adding a domain.

### 7. Give a project its own domain (optional)

A project can override the global template so one hoster serves
`dev.example.com` for one project and `demo.example.com` for another. Set it
in that project's **Domain** panel at `/p/<project>`, or:

```bash
curl -fsS -X PUT "$API/projects/myproj/domain" \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"hostname_template":"{branch}.demo.example.com"}'
```

**Each domain needs its own wildcard DNS record** (`*.demo.example.com`) and
its own certificate. With built-in TLS on, the renewal loop picks the new
domain up on its next pass — no restart — and a new row appears in the
certificate table. The same first-label rule applies. Detail in
[Per-project domains](#per-project-domains).

### 8. Add project environment variables and registry credentials

A real deployment usually needs both, and both are per-project:

- **Environment variables** — API keys and config you don't want in an image or
  in `hoster.json`. Set them in the **Environment** panel at `/p/<project>` or
  with `PUT /projects/<project>/vars/<KEY>`. They are injected on every deploy,
  target specific services or all of them, and **override** the same key in
  `hoster.json`. See [Project environment & secrets](#project-environment--secrets).
- **Registry credential** — one per project, used only when the image's
  registry host matches the stored one. Set it in the **Registry credential**
  panel or with `PUT /projects/<project>/registry`. See
  [Private registry credentials](#private-registry-credentials).

The project name must match `project` in that branch's `hoster.json`.

### 9. Deploy from CI

Each branch ships a `hoster.json`; CI builds and pushes the image, then makes
one `POST /deploy` call with the file's contents. A GitHub Actions example and
the full field reference are in [Deploying a project](#deploying-a-project).

If something misbehaves at any step, [Troubleshooting](#troubleshooting) lists
the failure modes by symptom.

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
| `HOSTER_PROJECTS_FILE` | `/etc/hoster/projects.json` | Where [project environment variables](#project-environment--secrets) are stored (`0600`). |
| `HOSTER_HTTPS_LISTEN` | *(unset)* | Address the **HTTPS listener** binds. Unset disables [built-in TLS](#built-in-tls) entirely — no listener, no renewal loop, no issuance. |
| `HOSTER_CERT_DIR` | `/var/lib/hoster/certs` | Where issued certificates and keys are stored. |
| `HOSTER_ACME_ACCOUNT_FILE` | `/var/lib/hoster/acme-account.json` | Where the Let's Encrypt account key is stored. |
| `HOSTER_ACME_PRODUCTION` | *(unset — staging)* | Set to `1`, `true`, or `yes` to request certificates from Let's Encrypt **production** instead of staging. See [Built-in TLS](#built-in-tls). |
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

This is the alternative to [Built-in TLS](#built-in-tls): instead of having
hoster obtain and terminate its own certificates, terminate TLS in front of it
with your own reverse proxy and forward requests, preserving the `Host`
header (hoster routes on it). Point the proxy listener at localhost and let
the terminator own the public ports:

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
dashboard with a navigation rail listing every project. It has three kinds of
page:

- **Overview** (`/`) — counts of projects, deployments, and running
  deployments, then **All deployments**: every branch with its status and URLs,
  each linking to its project page.
- **Project** (`/p/<project>`) — a **Deployments** panel (branch, status, URLs,
  and an expandable **config** view of the `hoster.json` each branch was
  deployed from), plus **Environment**, **Domain**, and **Registry credential**
  panels.
- **Settings** (`/settings`) — the **TLS & DNS** panel (ACME account, DNS
  provider, **Retry now**) and the **Certificates** table.

It is served on the **control API listener** (`HOSTER_API_LISTEN`) at:

- `GET /login`, `POST /login` — password login (sets a session cookie)
- `GET /` — the overview
- `GET /p/<project>` — a project's page
- `GET /settings` — TLS & DNS and the certificate table
- `POST /ui/destroy/<branch>` — tear a branch down from the UI
- `POST /ui/projects/<project>/vars` — add/replace a managed env var
- `POST /ui/projects/<project>/vars/<key>/delete` — delete one
- `POST /ui/projects/<project>/registry` — set/replace the registry credential
- `POST /ui/projects/<project>/registry/delete` — remove it
- `POST /ui/projects/<project>/domain` — set the project's hostname template
- `POST /ui/projects/<project>/domain/delete` — revert to the global default
- `POST /ui/acme/config`, `POST /ui/acme/dns`, `POST /ui/acme/dns/delete`,
  `POST /ui/acme/renew` — the TLS & DNS forms
- `POST /logout`

Because it lives on the private API listener, reach it the same way you reach
the control API: over your VPN/private interface, an SSH tunnel, or behind a
separate TLS-terminating, access-controlled vhost. Leave
`HOSTER_DASHBOARD_PASSWORD` unset to disable the dashboard entirely (its routes
then return `503 dashboard not configured`).

### Project environment & secrets

hoster can hold environment variables for a project — API keys, tokens, and
other config you don't want baked into an image or committed to `hoster.json`.
It injects them into that project's services on **every** deploy. Set them once
in the dashboard's **Environment** section (or via the API below); they persist
in `HOSTER_PROJECTS_FILE` (`/etc/hoster/projects.json`, mode `0600`).

- **Targeting.** Each variable targets specific services (a comma-separated
  list), or **all** services when left blank. A `GOOGLE_API_KEY` can go to
  `backend` only.
- **Precedence.** On a key conflict, the stored value **wins** over the same key
  in `hoster.json` — so central rotation always takes effect and a branch can't
  shadow a secret. Stored values are injected verbatim (no `{{…}}` templating).
- **Masking.** Values are write-only: the dashboard and API show a variable's
  key and target services but **never** its value.
- **The `project` must match.** Use the same name as `project` in the branch's
  `hoster.json`.

> **Trust note.** An injected value does end up in the target container's
> environment, so it's visible via `docker inspect` on the host — inherent, and
> consistent with hoster's "host access = full access" model. It never appears
> in container labels or logs.

JSON API (bearer token), for CI-driven rotation:

```bash
# set/replace a variable (services: [] = all services)
curl -fsS -X PUT "$API/projects/odinvestor/vars/GOOGLE_API_KEY" \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"value":"AIza…","services":["backend"]}'

# list (masked — values are never returned)
curl -fsS "$API/projects" -H "Authorization: Bearer $HOSTER_TOKEN"

# delete
curl -fsS -X DELETE "$API/projects/odinvestor/vars/GOOGLE_API_KEY" \
  -H "Authorization: Bearer $HOSTER_TOKEN"
```

### Private registry credentials

If a project's images live in a private registry, give the project a
credential and hoster authenticates its pulls with it. Set it once in the
dashboard's **Registry credential** panel (or via the API below): enter the
registry host, a username, and a token or password. For GitHub Container
Registry that's `ghcr.io`, your GitHub username, and a personal access token
with `read:packages`.

- **Host matching.** The credential is attached to a pull **only** when the
  image's registry host equals the stored one. A project holding a `ghcr.io`
  token still pulls `postgres:16` anonymously from Docker Hub, so the token
  never leaves the registry it belongs to. Docker Hub images (`postgres:16`,
  `library/postgres`, …) always normalize to the host `docker.io` — store the
  credential as `docker.io`, not `index.docker.io`, or it will silently never
  match.
- **One credential per project.** Saving a new one replaces the old.
- **Masking.** The password is write-only: the dashboard and API return the
  host and username — listed alongside variables in the same `GET /projects`
  response — but never the password.
- **Not verified.** hoster does not check the credential against the
  registry when you save it; a bad one shows up as a failed deploy with the
  registry's own error.

The password is stored in `HOSTER_PROJECTS_FILE` alongside project env vars
(mode `0600`) and, like them, is not encrypted at rest — see
[Project environment & secrets](#project-environment--secrets).

JSON API (bearer token):

```bash
# set/replace the credential
curl -fsS -X PUT "$API/projects/odinvestor/registry" \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"registry":"ghcr.io","username":"my-user","password":"ghp_..."}'

# remove it
curl -fsS -X DELETE "$API/projects/odinvestor/registry" \
  -H "Authorization: Bearer $HOSTER_TOKEN"
```

### Per-project domains

By default every branch of every project lands on `HOSTER_HOSTNAME_TEMPLATE`.
A project can override that with its own template, so one hoster can serve
`dev.example.com` for one project and `demo.example.com` for another.

In the dashboard, each project's **Domain** panel shows its effective template —
either its own, or the global default marked as the default — with a form to
change it.

Or through the API:

```bash
curl -fsS -X PUT "$API/projects/myproj/domain" \
  -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"hostname_template":"{branch}.demo.example.com"}'
```

`DELETE /projects/myproj/domain` reverts the project to the global default.

The template must contain `{branch}` — without it, every branch of the project
would resolve to one hostname and each deploy would displace the previous. It
must also include a parent domain (`{branch}.demo.example.com`, not bare
`{branch}`), and every placeholder must fall within the template's first
label — a TLS wildcard certificate only ever covers one label, so a template
that spreads placeholders across labels is rejected. `{service}` itself is
optional, so `{branch}.demo.example.com` works for a single-service project.

Changing a project's domain affects **subsequent** deploys only. Branches
already running keep the hostnames they were deployed with, because each
container records its own hostname; redeploy a branch to move it.

Each domain still needs its own wildcard DNS record, and its own certificate —
either from your own reverse proxy (see
[HTTPS with a reverse proxy](#https-with-a-reverse-proxy)) or from hoster's
own ACME client (see [Built-in TLS](#built-in-tls)).

### Built-in TLS

hoster can terminate TLS itself, issuing and renewing its own Let's Encrypt
certificates instead of sitting behind nginx or another reverse proxy.
Certificates are issued via the ACME **DNS-01** challenge, so they can be
wildcards — one certificate per domain covers every branch's hostname.
**Only Cloudflare is supported as a DNS provider today.**

This section is the reference. For the order to do it in on a new host — plain
HTTP first, then staging on a high port, then production — follow
[Setting up a host, end to end](#setting-up-a-host-end-to-end).

> **Not yet exercised in production.** hoster's ACME client has not been run
> against a live Let's Encrypt server. Treat the first issuance on any host as
> something to watch, start on staging, and check the certificate table before
> assuming it worked.

**1. Create a Cloudflare API token.** In the Cloudflare dashboard, scope it to
`Zone:DNS:Edit` on just the zone(s) hoster needs, not a global API key. hoster
only ever creates and deletes `_acme-challenge` TXT records.

**2. Enter the ACME email, control hostname, and token in the dashboard.**
Open `/settings` and use the **TLS & DNS** panel (requires
[`HOSTER_DASHBOARD_PASSWORD`](#the-dashboard)) to fill in:

- **ACME account** — your email and, optionally, a control hostname (a plain,
  non-wildcard hostname such as `hoster.example.com`). hoster issues a
  certificate for that name **and serves the API and dashboard on it over the
  HTTPS listener**, so the dashboard is reachable at
  `https://hoster.example.com` rather than only on the plain
  `HOSTER_API_LISTEN` port. Authentication is unchanged: API routes still
  require the bearer token, dashboard routes still require the dashboard
  password and its session cookie. Point the name's DNS at this host, or
  leave it unset if you don't want hoster reachable that way.
- **DNS provider** — `cloudflare` and the API token from step 1.

The same fields are available over the bearer-token API, for scripting:

```bash
curl -fsS -X PUT "$API/acme/config" -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"email":"you@example.com","control_hostname":"hoster.example.com"}'

curl -fsS -X PUT "$API/acme/dns" -H "Authorization: Bearer $HOSTER_TOKEN" \
  -d '{"kind":"cloudflare","token":"the-cloudflare-token"}'
```

The token is stored in `HOSTER_PROJECTS_FILE` under mode `0600`, the same as
project secrets, and **is never displayed again** once saved — the dashboard
and `GET /acme/status` show only that a provider is configured, never the
token itself.

**3. Set `HOSTER_HTTPS_LISTEN`** and restart hoster:

```bash
sudoedit /etc/hoster/hoster.env      # HOSTER_HTTPS_LISTEN=0.0.0.0:8443
sudo systemctl restart hoster
```

This starts the HTTPS listener and a background renewal loop that issues and
renews a certificate for every domain hoster currently wants one for: the
global `HOSTER_HOSTNAME_TEMPLATE`, every project's own domain override, and
the control hostname, if set. Certificates persist in `HOSTER_CERT_DIR`
(default `/var/lib/hoster/certs`) and outlive restarts — hoster does not
reissue a certificate that is already valid on disk.

Branch URLs reported by the API, injected into containers as `{{url.*}}`, and
linked from the dashboard switch to `https://` as soon as
`HOSTER_HTTPS_LISTEN` is set, so a frontend never calls its backend over plain
HTTP from an HTTPS page.

**Retrying after a configuration change.** A pass runs every six hours, which
is far too slow to tell you whether the token you just pasted works. The
**TLS & DNS** panel has a **Retry now** button that runs a pass immediately and
clears the backoff accumulated by failures the old configuration caused. The
same trigger is available for scripting:

```bash
curl -fsS -X POST "$API/acme/renew" -H "Authorization: Bearer $HOSTER_TOKEN"
```

Manual triggers are limited to **one every 20 minutes**, so a retry loop can't
spend the rate-limit budget faster than the backoff would. A request that
lands sooner returns `429` with the seconds remaining; one sent while TLS is
off returns `503 TLS is not enabled (HOSTER_HTTPS_LISTEN is unset)`. Failures
*after* a manual retry back off exactly as before — Let's Encrypt's rate
limits apply no matter who asked for the attempt.

**Staging by default.** Until you set `HOSTER_ACME_PRODUCTION` (to `1`,
`true`, or `yes`), hoster requests certificates from Let's Encrypt's
**staging** environment, whose certificates are **not trusted by browsers** —
you'll see a certificate warning until you switch to production. That's
deliberate, not a wart: it proves DNS-01, your Cloudflare token, and the
renewal loop all work end to end before you're spending production's rate
limits on a configuration that might still be wrong. Those limits are **5
failed validations per identifier per hour** and **50 certificates per
registered domain per week** — an hour's lockout for a bad token, up to a week
if you also burn the weekly budget reissuing. Once a staging certificate
issues cleanly, switch over:

```bash
sudoedit /etc/hoster/hoster.env      # HOSTER_ACME_PRODUCTION=1
sudo systemctl restart hoster
```

**4. Watch certificates appear in the certificate table.** The
**Certificates** panel on `/settings` lists one row per domain hoster wants a
certificate for, with a plain-language state: `pending`, `valid until <date>`,
or `failed: <reason>` (`GET /acme/status` returns the same rows as JSON, each
with a `severity` of `pending`, `valid`, or `failed`). **A domain without a
valid certificate keeps serving plain HTTP rather than going dark** — that is
the deliberate failure mode, so a bad token or a typoed hostname degrades one
domain instead of taking every branch down. The **Certificates** panel is the
only place you'll see it, so check it after changing DNS credentials or adding
a domain.

#### Cutting over from nginx

The same sequence as step 6 of
[Setting up a host, end to end](#setting-up-a-host-end-to-end), condensed for
an install that already serves traffic. To move an existing nginx-terminated
install to built-in TLS without a gap:

1. Configure the ACME account and Cloudflare token as above, staying on
   staging.
2. Set `HOSTER_HTTPS_LISTEN=0.0.0.0:8443` (or any free port) so hoster runs
   its HTTPS listener *alongside* nginx, which keeps `:443` for now.
3. Watch the certificate table until every domain you care about reads
   `valid`, then confirm a branch actually serves over it:
   ```bash
   openssl s_client -connect <host>:8443 -servername backend-main.dev.example.com </dev/null \
     | openssl x509 -noout -text | grep -A1 "Subject Alternative Name"
   ```
4. Set `HOSTER_ACME_PRODUCTION=1`, restart, and confirm a browser-trusted
   certificate is issued.
5. Move hoster onto the public port and retire nginx:
   ```bash
   sudoedit /etc/hoster/hoster.env    # HOSTER_HTTPS_LISTEN=0.0.0.0:443
   sudo systemctl restart hoster
   sudo systemctl stop nginx && sudo systemctl disable nginx
   ```
   Binding `:443` as the non-root `hoster` service user works because the
   installed unit already grants `CAP_NET_BIND_SERVICE` (see
   [Ports and binding](#ports-and-binding)).

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

- **Built-in TLS is opt-in and Cloudflare-only.** Leave `HOSTER_HTTPS_LISTEN`
  unset and hoster serves plain HTTP, as before. Set it, and hoster
  terminates TLS and manages Let's Encrypt certificates itself — but only
  Cloudflare is supported as a DNS provider, and each domain still needs its
  own wildcard DNS record and its own certificate. Issuance has **not yet been
  exercised against a live ACME server**, so start on staging. See
  [Built-in TLS](#built-in-tls).
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
| Certificate row reads `failed: …` mentioning authentication or a `403` | The Cloudflare token is wrong, revoked, or lacks `Zone:DNS:Edit`. A token scoped read-only, or to the wrong permission group, cannot write the `_acme-challenge` TXT record. Re-create it, save it in **TLS & DNS**, then **Retry now**. |
| Certificate row reads `failed: no zone found` | The domain's zone isn't in the Cloudflare account the token belongs to, or the token is scoped to other zones only. Confirm the zone is at Cloudflare and included in the token's scope. |
| `HOSTER_HTTPS_LISTEN` set, listener up, but every row stays `pending` and nothing issues | No DNS credential saved. The **TLS & DNS** panel shows `no token set` and issuance is skipped every pass. Add the ACME account *and* the Cloudflare token. |
| A row is stuck on `pending` | Issuance hasn't succeeded yet. A scheduled pass runs every six hours and failures back off, so click **Retry now** (at most one every 20 minutes) and check `journalctl -u hoster -f` for the ACME error. |
| Browser warns the certificate is untrusted, but the row reads `valid until …` | Expected on staging — Let's Encrypt staging certificates are deliberately not browser-trusted. Set `HOSTER_ACME_PRODUCTION=1` and restart once staging issues cleanly. |
| `POST /acme/renew` returns `503 TLS is not enabled` | `HOSTER_HTTPS_LISTEN` is unset, so no renewal loop is running to trigger. |
| `POST /acme/renew` returns `429` | A pass was requested less than 20 minutes ago; the response says how many seconds to wait. |
