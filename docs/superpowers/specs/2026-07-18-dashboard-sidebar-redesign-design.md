# Dashboard sidebar redesign + live logs

**Date:** 2026-07-18
**Status:** Approved

## Motivation

The current dashboard (`src/dashboard.rs`, one 607-line file) renders every
project as a stacked panel down a single scrolling page. Two problems:

1. **Navigation.** With more than a couple of projects the page becomes a long
   scroll with no sense of place. There is no way to focus on one project.
2. **Visual quality.** It reads as generic ("looks cheap") rather than as a
   polished developer console.

Additionally, operators currently have no way to see container logs from the
dashboard — they must SSH to the box and run `docker logs`.

## Goals

- Replace the single-scroll layout with a **master-detail** layout: a
  persistent left navigation rail + a right content pane.
- Give the console a **refined, distinctive visual identity**.
- Add **live-streaming container logs** to the project view.
- Keep the app **server-rendered with real URLs** — no SPA, no build step. The
  only client-side JavaScript is a small, scoped script for the live log view.

## Non-goals

- No editable operator settings. Server configuration is set at startup (env /
  flags); the Settings view is read-only.
- No client-side routing, no bundler, no framework.
- No changes to the bearer-token CLI API surface (`/deploy`, `/deployments`,
  `/projects`, teardown, var/registry endpoints).

## Navigation & routes

Every authenticated page renders the same frame: a left rail + a right content
pane. The left rail contains, top to bottom:

- Brand mark + wordmark
- **Overview** (link to `/`)
- `PROJECTS` label, then one link per project (`/p/{project}`)
- divider
- **Settings** (link to `/settings`)
- **Sign out** (existing `POST /logout` form)

The rail highlights the active item based on the current route.

| Method | Path                                   | View / action                                       |
|--------|----------------------------------------|-----------------------------------------------------|
| GET    | `/`                                    | **Overview**: aggregate counts + cross-project list of all deployments with status and quick links. |
| GET    | `/p/{project}`                         | **Project**: deployments (with live-log toggles), environment variables, registry credential. |
| GET    | `/settings`                            | **Settings**: read-only system info + Sign out.     |
| GET    | `/p/{project}/logs/{branch}/{service}` | **SSE stream** of live log lines (cookie auth).     |
| POST   | `/login` `/logout`                     | unchanged                                           |
| POST   | `/ui/destroy/{branch}`                 | unchanged action; redirect to `/p/{project}`        |
| POST   | `/ui/projects/{project}/...`           | unchanged actions; redirect to `/p/{project}`       |

The env/registry/destroy POST handlers change only their redirect target: back
to the originating `/p/{project}` instead of `/`.

### Overview content

- Header stats: number of projects, total deployments, number running.
- A flat list of every deployment across all projects, each showing project,
  branch, status, and its URLs — each row links to its `/p/{project}`.

### Settings content

Read-only presentation of the running server's configuration, drawn from
`Settings`: hostname template, registry host, listen / api-listen addresses,
and the build version. Plus the Sign out action. Values that are secrets
(token, dashboard password) are **not** shown.

## Live log streaming

### Runtime layer

`ContainerRuntime` gains one method:

```rust
async fn logs(
    &self,
    container_id: &str,
    follow: bool,
    tail: usize,
) -> anyhow::Result<LogStream>;
```

where `LogStream = Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>`
(each item is one already-decoded log line, no trailing newline).

- **Docker impl** (`docker.rs`): uses bollard's `logs` with
  `follow: true, tail: "<n>", stdout: true, stderr: true`, mapping each
  `LogOutput` chunk to its UTF-8 line(s).
- **Fake impl** (`runtime.rs` tests): returns a canned finite stream of a few
  lines so tests and no-Docker local dev work without a daemon.

### Engine layer

A method to resolve `(project, branch, service)` to a running container id
(reusing existing label lookups), then delegate to `runtime.logs(...)`. Returns
a not-found error when no such container exists.

### HTTP layer (SSE)

`GET /p/{project}/logs/{branch}/{service}`:

- Cookie-authenticated, same as the other `/ui` + page routes (redirect to
  `/login` when unauthenticated — for SSE, respond 401 rather than redirect).
- Response headers: `Content-Type: text/event-stream`,
  `Cache-Control: no-cache`, `Connection: keep-alive`.
- Body: a hyper streaming body that maps each log line to an SSE frame
  (`data: <escaped line>\n\n`). Ends when the underlying container log stream
  ends (container stops).

### Client layer

The project page renders each deployment's services with a **logs** toggle and
a hidden terminal-style panel. A small inline `<script>` (scoped to the project
page, ~30 lines):

- On expand: `new EventSource("/p/{project}/logs/{branch}/{service}")`, append
  each message as a line to the panel, auto-scroll to bottom.
- On collapse: `es.close()`.

No other page carries JavaScript.

## Code structure

`src/dashboard.rs` is replaced by a focused `src/ui/` module so each view stays
independently readable and testable:

- `ui/mod.rs` — module wiring + the `page()` HTML shell (`<head>`, `<title>`).
- `ui/style.rs` — the CSS (`STYLE` const).
- `ui/components.rs` — shared primitives: `html_escape`, status dot/led, chip,
  brand mark + icons, plural helper.
- `ui/shell.rs` — the app frame: left rail + right pane wrapper, active-item
  highlighting given the current route and project list.
- `ui/overview.rs` — the Overview page body.
- `ui/project.rs` — the Project page body (deployments, log panels, env,
  registry) — absorbs today's `render_deployments` / `render_config` /
  `render_environment` / `render_registry`.
- `ui/settings.rs` — the Settings page body.
- `ui/login.rs` — the login page.

Public entry points the API layer calls: `login_page`, `overview_page`,
`project_page`, `settings_page`.

## Visual direction

A refined developer-console identity (in the spirit of Vercel / Railway / Fly
dashboards), applied via `frontend-design` guidance during implementation:

- Dark-first, with the existing light-mode token set preserved.
- A calm, persistent left rail; generous whitespace; a real typographic scale.
- One restrained accent; monospace for identifiers, branches, and logs.
- Subtle borders and depth rather than heavy shadows.
- Status conveyed by a quiet color + dot, not loud pills.
- A true terminal look for the log panel (monospace, dimmed timestamps,
  dark surface).

The existing CSS-variable design tokens (`--bg`, `--panel`, `--accent`, status
colors, light-mode overrides, reduced-motion handling) are the starting point
and are refined, not discarded.

## Testing

- Update existing substring tests in the `ui` module for the new routes and
  per-view structure.
- Sidebar renders every project as a link and highlights the active one.
- Overview aggregates counts and lists deployments across projects.
- Project page renders deployments, env (masked), registry (password masked),
  and a log toggle per service.
- Settings page renders system info and never renders the token or password.
- SSE log endpoint: emits `text/event-stream` frames from the fake stream, and
  requires authentication (401 without a valid cookie).
- Existing HTML-escaping guarantees preserved across all views.
