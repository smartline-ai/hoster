# hoster dashboard — server-rendered status UI

**Status:** design approved, building
**Date:** 2026-07-18
**Builds on:** proxy-core (merged) and deploy-engine (merged).

## Goal

Turn the `404` at `hoster.odinvestor.net/` into a small web page that lists the
current branch deployments — branch, status, and clickable app URLs — with a
Destroy button per row. Server-rendered HTML, cookie login, no SPA.

## Scope

**In:** a login page (shared password → session cookie), a read-only dashboard
listing `engine.deployments()`, and a Destroy action. Served by the existing
control API on its own listener.

**Deferred to their proper milestones (not built here):**
- **Pin** — exempts a branch from TTL; TTL doesn't exist yet. Ships with the TTL
  milestone.
- **Logs** — container log streaming; a nice-to-have after the core UI.
- **Manual deploy from the UI** — deploys come from CI; the API already does it.

## Two doors, cleanly separated

The control API already authenticates machine callers with a **bearer token**
(`/deploy`, `/deployments`, `/healthz` — unchanged). Browsers can't send that by
hand, so the UI gets its own door: a **shared password → session cookie**.

- `HOSTER_DASHBOARD_PASSWORD` (new, optional) is the dashboard password.
- If it is unset, the UI routes return a "dashboard not configured" page and the
  API keeps working exactly as today. The dashboard is purely additive.
- The bearer-token API routes are untouched and never accept the cookie; the UI
  routes never accept the bearer token. No route honors both.

## Routes (added to the control API)

| method + path | auth | effect |
| --- | --- | --- |
| `GET /login` | none | render the login form |
| `POST /login` | password in form body | on match, create a session, set the cookie, 303 → `/`; on mismatch, re-render login with an error |
| `POST /logout` | cookie | drop the session, clear the cookie, 303 → `/login` |
| `GET /` | cookie | render the dashboard; no/invalid cookie → 303 → `/login` |
| `POST /ui/destroy/{branch}` | cookie | `engine.teardown(branch)`, 303 → `/` |

Everything else stays as-is: `POST /deploy`, `DELETE /deploy/{branch}`,
`GET /deployments` (bearer), `GET /healthz` (open).

`GET /` previously 404'd; it now serves the dashboard or redirects to login.

## Sessions

A session is a random 256-bit token. On successful login the server stores the
token in an in-memory set and sets it as a cookie:

```
Set-Cookie: hoster_session=<token>; HttpOnly; Secure; SameSite=Lax; Path=/; Max-Age=…
```

- `HttpOnly` (no JS access), `Secure` (TLS-only — we're behind HTTPS),
  `SameSite=Lax` (CSRF defense for the destroy POST).
- The store is a `Mutex<HashSet<String>>`. Sessions are process-lived: a hoster
  restart logs everyone out (acceptable — they log back in). No persistence, no
  DB, consistent with the milestone's "labels not database" stance.
- Password comparison is constant-time (the login endpoint is public;
  don't leak length/prefix via timing).

## Modules

| File | Responsibility |
| --- | --- |
| `src/dashboard.rs` | Pure HTML rendering: `login_page`, `dashboard_page`, and HTML-escaping. No hyper, no IO. |
| `src/api.rs` (extend) | The UI routes, cookie parsing/setting, session store, password check. |
| `src/settings.rs` (extend) | `dashboard_password: Option<String>`. |

`dashboard.rs` is pure and separately testable — it takes data and returns a
`String`. All the HTTP/session logic stays in `api.rs`.

## Security

- **XSS:** every value interpolated into HTML (branch, status, URLs) is
  HTML-escaped. Statuses can contain free text (`failed: <message>` derived from
  image names / errors), so escaping is mandatory and tested with a `<script>`
  payload.
- **CSRF:** the destroy action is a `POST` with a `SameSite=Lax` session cookie,
  so a cross-site form can't drive it.
- **Cookie theft:** `HttpOnly` + `Secure`.
- **Brute force:** constant-time compare + the operator is told to use a strong
  password. Rate-limiting is out of scope for this milestone (single internal
  team, and the CI token — the dangerous capability — is a separate, stronger
  secret).
- The dashboard can `teardown` branches but cannot deploy — it never accepts a
  config, so the UI's blast radius is strictly "destroy an existing branch."

## Testing

- **dashboard.rs** — pure unit tests: login page renders a form; dashboard page
  lists rows with branch/status/URL; a `<script>` in a status field comes out
  escaped; empty deployments renders an empty-state message.
- **api.rs** — integration tests against `serve_api` backed by `FakeRuntime`:
  - `GET /` without cookie → 303 to `/login`.
  - `POST /login` wrong password → 200, no session cookie set.
  - `POST /login` right password → 303 + `Set-Cookie: hoster_session=`.
  - `GET /` with the returned cookie → 200 and the dashboard HTML.
  - `POST /ui/destroy/{branch}` with cookie → 303 and the branch is gone.
  - `POST /ui/destroy` without cookie → 303 to `/login`, no teardown.
  - With `dashboard_password = None`, `GET /login` → 503 "not configured".
  - The bearer API (`GET /deployments`) still works and does NOT accept the
    session cookie.

## Operator setup (after build)

- Set `HOSTER_DASHBOARD_PASSWORD` in `/etc/hoster.env`, `systemctl restart
  hoster`. Rebuild the binary and redeploy to the server. `hoster.odinvestor.net`
  then shows the login page.

## Build order

1. `dashboard.rs` — pure HTML rendering + escaping.
2. Session store + cookie helpers + `settings.dashboard_password`.
3. UI routes wired into `serve_api`, with the integration tests.
