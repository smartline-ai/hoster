# Hand-off: pluggable reverse-proxy backend (standalone vs. nginx)

Date: 2026-07-19
Status: **queued, not started.** No code or spec written yet. This is the
second subsystem split off during the DNS brainstorming; the DNS providers
subsystem is done and merged to `main` (commit `f173949`).

Next action: run `superpowers:brainstorming` to resolve the one open design
fork below (TLS ownership), then write a spec → plan → execute via
`superpowers:subagent-driven-development`, exactly as the DNS work was done.

---

## What the user asked for

> "I also want to be able to run it on Nginx or [standalone] instead of nginx."

Clarified during brainstorming (2026-07-19) to **"Standalone or nginx backend"**:

- **Mode A — standalone (today's behavior):** hoster binds `:80`/`:443` itself
  and is the edge reverse proxy.
- **Mode B — nginx backend (new):** hoster writes nginx vhost config (e.g.
  `/etc/nginx/conf.d/hoster-*.conf`) and reloads nginx; **nginx is the edge**,
  proxying to hoster/containers. The operator picks the mode per install.

Explicitly rejected: adding Caddy (or other named proxies); an "external proxy
only" design that drops the built-in proxy. Standalone stays as a first-class
option.

## Current architecture (what Mode B has to slot into)

hoster **is** the edge proxy today. Relevant surface (line numbers will drift):

- `src/main.rs` — binds `proxy_listener` = `settings.listen`
  (`HOSTER_LISTEN`, default `127.0.0.1:8080`, set `0.0.0.0:80` for public),
  `api_listener` = `settings.api_listen` (`HOSTER_API_LISTEN`), and, when
  `settings.https_listen` (`HOSTER_HTTPS_LISTEN`) is set, spawns `serve_https`
  which terminates TLS itself. Spawns `proxy::serve(proxy_listener, routes)`.
- `src/proxy.rs` — `pub async fn serve(listener, routes: SharedRoutes)` and
  `pub async fn handle(req, routes, client, scheme)`. Routes public requests by
  the **Host header** to a container upstream. Plain HTTP and the HTTPS path
  share the same routing + `handle`.
- `src/routing.rs` — `RoutingTable { HashMap<String, Route> }`, `Route { upstream:
  SocketAddr, state: RouteState }`, `lookup(host)`. `SharedRoutes` is an
  arc-swapped table so the route set hot-swaps without restart.
- `src/engine.rs` — `deploy`/`teardown`/`reconcile` rebuild the routes from
  running containers. Upstreams come from container **labels** (`src/labels.rs`:
  `hoster.hostname`, `hoster.port`, `hoster.branch`, `hoster.service`,
  `hoster.project`) plus the container IP. Hostnames are computed by
  `settings::hostname_for(template, service, branch)`.
- `src/tls.rs` — SNI cert resolver, hot-swappable (`arc-swap`); built-in ACME
  lives in `src/acme.rs` + `src/renewal.rs`; certs on disk in `settings.cert_dir`.
- `Settings` fields today (`src/settings.rs`): `listen`, `api_listen`,
  `hostname_template`, `registry`, `token`, `dashboard_password`,
  `https_listen`, `cert_dir`, `public_ip`.
- **No proxy-mode / nginx notion exists yet.** `docs/deploying.md`'s "nginx
  cutover" is only about pointing an *existing* operator nginx at hoster — there
  is zero nginx config generation today.

## THE central design fork (resolve in brainstorming first)

**Who terminates TLS in nginx mode, and what does hoster generate?** These are
coupled. Two coherent shapes; pick one before anything else.

**Option 1 — nginx terminates TLS, proxies everything to hoster (recommended
starting point).** hoster keeps its built-in ACME/DNS-01 issuance (unchanged,
proxy-agnostic) and keeps routing by Host. In Mode B it generates ONE nginx
server block that `proxy_pass`es all traffic to hoster's plain HTTP listener,
wiring in the hoster-issued cert paths for `ssl_certificate`. nginx handles
TLS + HTTP/2 + edge concerns; hoster stays the Host router. Smallest change:
one static-ish server block, regenerated only when certs/hostnames change, not
per deploy. Keeps the whole routing/labels/hot-swap machinery intact.

**Option 2 — nginx routes directly to containers (bypass hoster's proxy).**
hoster regenerates nginx `upstream`/`server` blocks on every deploy/teardown
(one per branch hostname → container `ip:port`) and reloads nginx. Removes a
network hop, but couples nginx config churn to the deploy hot path, needs an
`nginx -t` + reload on every branch change, and duplicates the routing logic
that `RoutingTable` already does. More moving parts, more failure surface.

Recommendation: **Option 1.** It makes Mode B a thin edge/TLS layer and reuses
everything. Only reach for Option 2 if dropping the hoster hop is a hard
requirement.

## Other design questions for the spec

- **Mode selection:** a new setting, e.g. `HOSTER_PROXY_MODE=standalone|nginx`
  (default `standalone` = today). In `nginx` mode, decide what happens to
  `HOSTER_LISTEN`/`HOSTER_HTTPS_LISTEN` (likely: hoster binds only a local HTTP
  listener for nginx to proxy to; hoster stops binding `:443`).
- **Config generation:** target dir (`/etc/nginx/conf.d/hoster-*.conf`), atomic
  write, then **`nginx -t` validate before reload** (never reload an invalid
  config), then `nginx -s reload` / `systemctl reload nginx`. Clean up the
  generated file(s) on teardown/mode-switch.
- **Reload permissions:** hoster needs rights to write the conf dir and reload
  nginx (runs as root, or a narrow sudoers entry). Document it; fail loudly with
  a clear message when it can't.
- **Failure semantics:** mirror the DNS work's discipline — a config-write or
  reload failure should be surfaced (dashboard/logs) and, ideally, non-fatal to
  the deploy where reasonable, but here a bad reload can take the whole edge
  down, so `nginx -t`-before-reload is mandatory and a failed validate must
  abort the reload and keep the last-good config.
- **Interaction with the built-in TLS/ACME/DNS work (already shipped):** ACME
  DNS-01 issuance and the new wildcard-A automation are **proxy-agnostic** and
  reusable as-is in nginx mode (certs still issued via DNS; wildcard A still
  points the base at the box). In Option 1, hoster hands nginx the cert paths it
  already manages. Don't re-solve TLS issuance for nginx mode.
- **UI/API:** surface the proxy mode + nginx status (config path, last reload
  result, `nginx -t` output) the way the DNS panel surfaces provider state.
- **Testing:** generate config to a temp dir and assert its contents; stub the
  `nginx -t`/reload command behind a seam (the DNS work used a swappable
  `dns_provider_builder` field on `Engine` for exactly this — mirror that
  pattern for the reload command so tests don't shell out to a real nginx).

## Reusable patterns from the DNS work (just merged)

- SDD flow worked well: 12 bite-sized TDD tasks, fresh implementer subagent per
  task, per-task spec+quality review, Opus for the risky ones, final
  whole-branch Opus review. Ledger at `.superpowers/sdd/progress.md`.
- Swappable-seam-for-testing pattern: a boxed default fn field on `Engine`
  (`dns_provider_builder`) with a `#[cfg(test)]` setter let tests drive the real
  `deploy()` path without side effects. Use the same for the nginx reload/`-t`
  command.
- Secret/output discipline in this codebase is strict (hand-written redacting
  `Debug`, escape-once in UI). nginx config generation is lower-secret but the
  same care applies to any operator-controlled value written into a config file
  (avoid config injection via hostnames/paths).

## Suggested first step for the next session

1. `superpowers:brainstorming` — resolve the TLS-ownership fork (Option 1 vs 2)
   and the mode-selection/permission questions with the user.
2. Write the spec to `docs/superpowers/specs/YYYY-MM-DD-reverse-proxy-backend-design.md`.
3. Plan (`superpowers:writing-plans`) → execute (`superpowers:subagent-driven-development`).
