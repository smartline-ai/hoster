# Design: pluggable reverse-proxy backend (standalone vs. nginx)

Date: 2026-07-19
Status: **approved design, not yet planned.** Supersedes the queued hand-off at
`docs/superpowers/handoffs/2026-07-19-reverse-proxy-backend-handoff.md`.

## Summary

hoster is the edge reverse proxy today: it binds `:80`/`:443`, terminates TLS,
and routes public requests to container upstreams by the `Host` header. This
adds a second, opt-in **proxy mode** where **nginx is the edge** and hoster sits
behind it.

The operator picks the mode per install via `HOSTER_PROXY_MODE`:

- **`standalone` (default, today's behavior):** hoster binds `:80`/`:443` and is
  the edge proxy. Nothing changes.
- **`nginx` (new):** nginx terminates TLS and reverse-proxies **all** traffic to
  hoster's plain HTTP listener; hoster keeps routing by `Host`. hoster generates
  one nginx server block per wildcard base and reloads nginx.

This is the resolution of the "TLS ownership" fork from the hand-off:
**Option 1 — nginx terminates TLS, proxies everything to hoster.** hoster keeps
its wildcard ACME/DNS-01 issuance and its `RoutingTable` machinery unchanged;
nginx mode is a thin edge/TLS layer in front. Option 2 (nginx routing directly
to containers, regenerating config per deploy) was rejected: it couples nginx
config churn to the deploy hot path and duplicates the routing logic
`RoutingTable` already owns.

## Why Option 1 needs no per-branch nginx config

hoster issues **wildcard** certificates (`src/acme.rs`, `src/renewal.rs`): one
cert covers `*.dev.example.com` and its parent, derived from the hostname
template via `settings::wildcard_base`. Because the cert and the nginx
`server_name` are both wildcard, a single nginx server block serves every branch
subdomain under a base.

When a branch is pushed, hoster adds `feat-x.dev.example.com` to its in-memory
`RoutingTable` and hot-swaps it in (as it does today). nginx is **never
touched** — it blindly forwards to hoster, which does the `Host` routing. nginx
config is regenerated only on rare operator/cert events, never per deploy.

## Settings

New field on `Settings` (`src/settings.rs`):

- `proxy_mode: ProxyMode` — enum `{ Standalone, Nginx }`, from
  `HOSTER_PROXY_MODE` (`standalone` | `nginx`), **default `Standalone`**.
  Unknown values are a hard startup error with a clear message.

New nginx-only settings, consulted **only** in `Nginx` mode:

- `nginx_conf_path: String` — from `HOSTER_NGINX_CONF`, default
  `/etc/nginx/conf.d/hoster.conf`. The single generated file.
- `nginx_reload_cmd: String` — from `HOSTER_NGINX_RELOAD_CMD`, default
  `systemctl reload nginx`. Validation is always `nginx -t` first (not
  configurable).

Backward compatibility: with `HOSTER_PROXY_MODE` unset, every existing install
behaves exactly as today. The new fields are inert in `Standalone` mode.

## Listener behavior by mode

`main.rs` today binds `proxy_listener` (`HOSTER_LISTEN`), `api_listener`
(`HOSTER_API_LISTEN`), and — when `HOSTER_HTTPS_LISTEN` is set — spawns
`serve_https` which terminates TLS.

- **`Standalone`:** unchanged.
- **`Nginx`:** hoster binds **only** `HOSTER_LISTEN` (plain HTTP) and
  `HOSTER_API_LISTEN`. It does **not** spawn `serve_https` and does **not** bind
  `:443`, even if `HOSTER_HTTPS_LISTEN` is set — that value is ignored with an
  `info` log noting nginx owns TLS. ACME/renewal still runs so hoster keeps
  issuing and renewing the wildcard certs that nginx serves.

## New module: `src/nginx.rs`

### Pure renderer

```rust
pub struct NginxBase {
    pub server_name: String, // e.g. "dev.example.com" -> matches ".dev.example.com"
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

/// Render the full contents of the hoster nginx conf file.
pub fn render(bases: &[NginxBase], upstream: &SocketAddr) -> String;
```

- One shared `:80` `server` block that `proxy_pass`es to `upstream` (hoster's
  plain listener) with `proxy_set_header Host $host;` — serves the box during
  first issuance and lets hoster own any HTTP→HTTPS behavior.
- One `:443 ssl http2` `server` block **per base that has a cert on disk**, with
  `server_name .<base>;` (matches the wildcard and the parent), the base's
  `ssl_certificate` / `ssl_certificate_key`, and the same `proxy_pass` +
  `proxy_set_header Host $host;`. A base whose cert does not yet exist is
  omitted, so `nginx -t` still passes mid-first-issuance.
- Pure function of its inputs: unit-tested by asserting on the returned string
  (server_name lines, cert paths, upstream, omission of certless bases).

### Config injection guard

`server_name` and cert paths come from operator-controlled wildcard bases and
hoster-managed cert paths — not per-branch user input. The renderer still
validates each base against a strict hostname charset (`[a-z0-9.-]`, label
rules) and **skips + logs** any base that fails, so nothing unexpected is ever
written into the config file.

### Apply (write / validate / reload) with a test seam

```rust
pub struct NginxBackend {
    conf_path: PathBuf,
    reload_cmd: Vec<String>,     // parsed from HOSTER_NGINX_RELOAD_CMD
    runner: CommandRunner,       // test seam
}

pub struct ApplyOutcome {
    pub validated: bool,
    pub reloaded: bool,
    pub message: Option<String>, // captured nginx -t / reload stderr on failure
}

impl NginxBackend {
    pub fn apply(&self, config: &str) -> anyhow::Result<ApplyOutcome>;
}
```

`apply` steps:

1. Read the current file contents into memory (last-good backup); missing file
   is treated as empty backup.
2. Atomically write `config` to `conf_path` (reuse `certs::write_atomic`, mode
   `0o644`).
3. Run `nginx -t` via `runner`. **On failure:** restore the backup atomically,
   do **not** reload, return `ApplyOutcome { validated: false, .. }` carrying the
   captured stderr (the previously-loaded config keeps serving).
4. On pass: run `reload_cmd` via `runner`. Capture success/failure into
   `ApplyOutcome`.

`runner: Box<dyn Fn(&[&str]) -> anyhow::Result<CmdOutput> + Send + Sync>` is the
**test seam**, mirroring the `dns_provider_builder` pattern on `Engine`. The
default spawns the real process; a `#[cfg(test)]` constructor (e.g.
`NginxBackend::with_runner`) injects a stub so tests exercise the full
write/validate/reload path against a temp `conf_path` with **no real nginx**.

## Lifecycle — when config is (re)generated

The payoff of Option 1: **never on deploy/teardown.** hoster's `RoutingTable`
hot-swap handles every branch; nginx does not see branch churn. `apply()` runs
only at:

- **Startup** (`Nginx` mode): build `NginxBase`s from the configured wildcard
  bases (`settings::wildcard_base` over the hostname template(s)) and the certs
  currently on disk in `cert_dir`, `render`, then `apply`.
- **Cert rotation:** after `renewal.rs` saves a newly-issued or renewed cert,
  re-render and `apply` again. This is idempotent and picks up a base whose
  `:443` block appears for the first time once its cert exists, and reloads nginx
  so it loads fresh cert bytes (cert file paths are stable across renewals, so
  the file content is usually identical and only the reload matters).

Wiring detail for the plan: the renewal loop must be able to trigger an
`apply()`. `NginxBackend` is constructed once at startup in `Nginx` mode and
shared (e.g. `Arc`) with whatever drives renewal, so a rotation can call it.

## Failure semantics (mirrors the DNS work)

- `nginx -t` failure **never** reloads and **never** leaves a broken file live
  (backup restored) — last-good config keeps serving. Mandatory.
- **Startup** `apply` failure: log loudly and surface in status, but **non-fatal**
  to the hoster process — hoster keeps running its HTTP listener so the operator
  can fix nginx and hoster stays up.
- **Rotation** `apply` failure: surfaced in status/logs; the old cert and old
  config keep serving.
- All failures capture and surface the underlying `nginx -t` / reload stderr so
  the operator sees exactly what nginx rejected.
- Reload permissions: hoster needs rights to write `nginx_conf_path` and run the
  reload command (run as root, or a narrow sudoers entry for
  `systemctl reload nginx` + `nginx -t`). Documented in `docs/deploying.md`;
  when it can't, `apply` fails loudly with a clear, actionable message.

## UI / API

Mirror how the DNS provider panel surfaces state — **read-only** (proxy mode is
set via env at deploy time, like the other deploy settings, not toggled from the
dashboard):

- current `proxy_mode`,
- generated conf path,
- last `apply` result: validated? reloaded? timestamp,
- captured `nginx -t` output when the last apply failed.

Exposed on the existing status/API surface the DNS panel uses, rendered with the
same escape-once discipline for any operator-controlled string.

## Testing

- **Renderer:** pure-function unit tests — assert server_name lines, cert paths,
  upstream address, the shared `:80` block, and that certless bases are omitted.
- **Apply path:** `NginxBackend::with_runner` injects a stub `runner` that
  records invoked commands and returns canned success/failure. Tests cover: happy
  path (write → validate ok → reload ok), `nginx -t` failure (backup restored,
  no reload, stderr surfaced), and reload failure. All against a temp
  `conf_path`; no real nginx is ever spawned.
- **Settings:** parsing of `HOSTER_PROXY_MODE` (incl. default and unknown-value
  error) and the nginx-only env vars.
- **Injection guard:** a base with a bad hostname is skipped and logged, not
  written.

## Out of scope (YAGNI)

- Caddy or any other named proxy.
- Option 2 (nginx routing directly to containers; per-deploy config
  regeneration).
- Custom apex domains outside a configured wildcard base.
- Any change to ACME / DNS-01 issuance or the wildcard-A automation — reused
  as-is; both are proxy-agnostic.
- Toggling proxy mode from the dashboard — it is an env/deploy-time setting.
- `standalone` mode behavior — untouched, stays first-class.

## Files touched (anticipated)

- `src/settings.rs` — `ProxyMode` enum, `proxy_mode`, `nginx_conf_path`,
  `nginx_reload_cmd`; env parsing; a helper to enumerate wildcard bases.
- `src/nginx.rs` — **new** module: `render`, `NginxBase`, `NginxBackend`,
  `ApplyOutcome`, `CommandRunner` seam.
- `src/main.rs` — mode-conditional listener wiring; construct `NginxBackend` and
  run startup `apply` in `Nginx` mode; share it with the renewal path.
- `src/renewal.rs` — trigger `apply()` after a cert is saved.
- Status/API + dashboard template — surface proxy-mode/nginx status.
- `docs/deploying.md` — nginx-mode setup, permissions/sudoers, env vars.
