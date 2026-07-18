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
                                                      │  swaps routing table
                                                      ▼
   browser ──http──▶  hoster proxy (:80) ──Host header──▶ container
      backend-mybranch.dev.example.com  →  10.x.x.x:8080
```

> **Status.** hoster serves **plain HTTP** (no built-in TLS) and authenticates
> the control API with **one shared token**. It targets internal testing
> environments. Put a TLS-terminating reverse proxy in front for HTTPS. See
> [Limitations](docs/deploying.md#limitations).

---

## Install on a server (operators)

On a Linux host with Docker and systemd, one command installs the release
binary, a hardened systemd service, and a config file, then starts it:

```bash
curl -fsSL https://raw.githubusercontent.com/smartline-ai/hoster/main/scripts/install.sh | sudo sh
```

Pin a version instead of taking the latest:

```bash
curl -fsSL https://raw.githubusercontent.com/smartline-ai/hoster/main/scripts/install.sh | sudo VERSION=v0.1.0 sh
```

The installer:

- downloads the static `x86_64-linux-musl` binary to `/usr/local/bin/hoster` and
  **verifies its SHA-256 checksum**,
- creates a `hoster` system user and adds it to the `docker` group,
- writes `/etc/hoster/hoster.env` (only if absent — it never overwrites your
  token) with an auto-generated `HOSTER_TOKEN`,
- installs `/etc/systemd/system/hoster.service` and runs `systemctl enable --now`.

Re-running upgrades the binary and restarts the service; your config is left
alone. `sudo sh install.sh --uninstall` removes the service and binary
(`--purge` also removes `/etc/hoster` and the user).

### After installing

```bash
sudo systemctl status hoster        # is it up?
sudo journalctl -u hoster -f        # logs
sudoedit /etc/hoster/hoster.env     # set token, listen addrs, hostname template
sudo systemctl restart hoster       # apply config changes
curl -fsS http://127.0.0.1:8081/healthz   # -> ok
```

Then point **wildcard DNS** at the host (e.g. `*.dev.example.com` → host IP),
set `HOSTER_HOSTNAME_TEMPLATE` to match, and — if you need HTTPS — front hoster
with nginx or another TLS terminator. Configuration reference and the full app
workflow live in **[docs/deploying.md](docs/deploying.md)**.

---

## Configuration

hoster is configured entirely through environment variables (the installer puts
them in `/etc/hoster/hoster.env`):

| Variable | Default | Meaning |
| --- | --- | --- |
| `HOSTER_TOKEN` | *(required)* | Shared bearer token CI sends to the control API. |
| `HOSTER_LISTEN` | `127.0.0.1:8080` | Public proxy bind address. Installer defaults it to `0.0.0.0:80`. |
| `HOSTER_API_LISTEN` | `127.0.0.1:8081` | Control API bind address. Keep it private. |
| `HOSTER_HOSTNAME_TEMPLATE` | `{service}-{branch}.dev.example.com` | How public hostnames are built. |
| `HOSTER_REGISTRY` | `localhost:5000` | Registry base for the `{{registry}}` template variable. |
| `DOCKER_HOST` | *(Docker default)* | Set if your Docker socket is non-standard. |

---

## Build from source

Requires a recent Rust toolchain (edition 2024).

```bash
cargo build --release
./target/release/hoster
```

For a portable static binary matching the release artifact:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

---

## Releasing (maintainers)

Releases are cut from git tags. Bump `version` in `Cargo.toml`, then:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The [`release`](.github/workflows/release.yml) workflow verifies the tag matches
`Cargo.toml`, builds the musl binary, and publishes a GitHub Release with the
binary, a `.tar.gz` bundle (binary + installer), and `SHA256SUMS`. The
[`ci`](.github/workflows/ci.yml) workflow runs fmt, clippy, build, tests, and
shellcheck on every pull request and push to `main`.

---

## Documentation

- **[docs/deploying.md](docs/deploying.md)** — deploy your project with hoster:
  `hoster.json`, the control API, wiring it into CI, and known limitations.
