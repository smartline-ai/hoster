# Built-in TLS with automatic certificates

**Date:** 2026-07-19
**Status:** Approved, not yet implemented
**Branch:** `networking`
**Supersedes:** `2026-07-19-multi-domain-routing-design.md` — that slice is absorbed
here as phases 1–3. Its implementation plan
(`docs/superpowers/plans/2026-07-19-multi-domain-routing.md`) stays valid and is
reused verbatim for those phases; only its Task 1 validation rule changes, to
additionally require that all placeholders sit within the template's first label.

## Problem

hoster speaks plain HTTP. HTTPS requires an operator to hand-write an nginx
server block, obtain a wildcard certificate through a DNS-01 challenge, and wire
up renewal. Every additional domain repeats the work.

Two limits compound it. `Settings::hostname_template` is a single global string,
so every project is forced onto one domain. And nothing in hoster knows which
domains exist, so nothing can obtain certificates for them.

**Goal:** hoster serves HTTPS on its own, obtaining and renewing certificates
automatically, for several domains, configured through the dashboard. nginx
becomes unnecessary.

## Scope

- Per-project hostname templates (multi-domain routing).
- A `DnsProvider` trait with a Cloudflare implementation.
- ACME certificate issuance and renewal over DNS-01, using `instant-acme`.
- TLS termination with SNI-based certificate selection.
- Dashboard configuration for all of the above.

Out of scope: additional DNS providers (Hetzner, Namecheap — the trait exists so
they slot in later), HTTP/2, per-deploy or per-environment domain selection,
and certificate types other than Let's Encrypt.

## Configuration model

Everything configurable moves to the dashboard **except** what must be known
before a UI exists:

| Setting | Where | Why |
|---|---|---|
| `HOSTER_LISTEN`, `HOSTER_API_LISTEN` | env | Needed to bind at boot |
| `HOSTER_HTTPS_LISTEN` | env | Needed to bind at boot; absent means no TLS listener |
| `HOSTER_HOSTNAME_TEMPLATE` | env | Fallback default for projects with no domain |
| `HOSTER_CERT_DIR` | env | Certificate storage root, read at startup |
| `HOSTER_TOKEN`, `HOSTER_DASHBOARD_PASSWORD` | env | Bootstrap credentials |
| ACME account email | dashboard | Not needed until issuance |
| DNS provider + API token | dashboard | Not needed until issuance |
| Control hostname | dashboard | Not needed until issuance |
| Per-project domain | dashboard | Per-project data |

hoster therefore always boots and always serves. An unconfigured install serves
plain HTTP and shows a setup prompt.

## Architecture

Five modules, each independently testable, plus the routing change.

### 1. `src/dns.rs` — DNS provider abstraction

```rust
#[async_trait]
pub trait DnsProvider: Send + Sync {
    /// Publish a TXT value at `name`, leaving other values at that name intact.
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    /// Remove exactly this TXT value at `name`, leaving others intact.
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
}
```

`name` is **fully qualified** at the trait boundary
(`_acme-challenge.dev.example.com`). Each provider converts to whatever its API
expects — Cloudflare wants the full name, others want it relative to the zone.
That mismatch is the single most common cause of a record that appears to save
but never resolves, so each provider carries a direct test for it.

Both operations preserve unrelated values at the same name. A certificate
covering `dev.example.com` and `*.dev.example.com` produces **two simultaneous
TXT values at the same `_acme-challenge.dev.example.com`**; an implementation
that overwrites rather than appends breaks wildcard issuance.

`CloudflareProvider` is the only implementation in this slice. It resolves the
zone ID for a name once and caches it, then uses per-record CRUD by ID.

### 2. `src/acme.rs` — certificate issuance

Wraps `instant-acme` (the only maintained Rust ACME crate with a DNS-01 path;
`rustls-acme` is HTTP-01/TLS-ALPN only). Owns:

- ACME account creation, with the account credentials persisted as JSON so
  restarts reuse the account rather than registering a new one.
- Order creation for a domain's identifier set.
- Publishing challenge TXT records through the `DnsProvider`.
- **Waiting for propagation** by querying the zone's authoritative nameservers
  directly until they return the expected value, with a timeout — not a fixed
  sleep. Fixed sleeps are the main source of flaky DNS-01 and burn the
  authorization-failure rate limit.
- Polling order readiness and the issued certificate.
- Cleaning up challenge records afterwards, including on the failure path.

It knows nothing about hoster's routing or storage; it returns a certificate
chain and private key.

### 3. `src/certs.rs` — certificate store

Persists to `/var/lib/hoster/certs/<domain>/{fullchain.pem,key.pem}`, `0600`,
written atomically. Directory root configurable via `HOSTER_CERT_DIR`.

Responsibilities: load existing certificates at startup, parse their expiry,
and answer *which domains need work* — a domain with no certificate, or one
within 30 days of expiry. Certificates survive restarts; hoster never reissues
on boot when valid certificates exist on disk.

### 4. `src/tls.rs` — TLS termination

A rustls `ServerConfig` whose `ResolvesServerCert` selects a certificate by SNI,
including wildcard matching (`backend-main.dev.example.com` → `*.dev.example.com`).
The resolver sits behind an `ArcSwap`, mirroring the existing `SharedRoutes`, so
the renewal loop swaps in new certificates without dropping connections.

A domain with no certificate is simply absent from the resolver. The TLS
handshake for it fails, and the domain keeps serving over plain HTTP — the
chosen degradation. Nothing goes dark because of a certificate problem.

### 5. Renewal loop

A background task, started with the TLS listener. Every 6 hours it asks the
certificate store what is due, issues through the ACME module, writes, and swaps
the resolver.

Failure handling is a correctness requirement, not polish. Let's Encrypt allows
5 authorization failures per identifier per hour, 5 duplicate certificates per
identical name set per week, and 50 certificates per registered domain per week
counted globally across accounts. Note that `dev.example.com` and
`demo.example.com` draw from the same `example.com` bucket. So: exponential
backoff per domain starting at 15 minutes and capping at 24 hours, a failure
count and last error recorded per domain, and no retry of a domain whose backoff
has not elapsed.

### 6. Multi-domain routing

The per-project store gains an optional `hostname_template`. `Engine` resolves a
project's template at deploy time, falling back to the global default.

The resolved hostname is already written to the `hoster.hostname` container
label, and routing is rebuilt from labels — so the proxy and reconciliation
paths need no changes. A running branch keeps its hostname when its project's
template changes; only later deploys move.

`deploy`, `plan_urls`, and `urls_for` currently contain three byte-identical
URL-building loops. They collapse into one project-aware call site rather than
having the project threaded through three copies.

## Which domains get certificates

Derived, never configured twice: the distinct hostname templates across all
projects, plus the global default, each reduced to its wildcard base, plus the
control hostname as a single-name certificate.

Reduction: take the template's first label — the one containing the
placeholders — and replace it with `*`. `{service}-{branch}.dev.example.com`
becomes `*.dev.example.com`.

**A wildcard matches exactly one label.** A template whose placeholders span two
labels, such as `{branch}.{service}.dev.example.com`, would need `*.*.…`, which
Let's Encrypt will not issue. Templates must therefore keep every placeholder
within the first label, and validation enforces it.

Each wildcard certificate also includes the bare parent name as a second
identifier (`*.dev.example.com` plus `dev.example.com`), which is what produces
the two-simultaneous-TXT-values case above.

## Template validation

A template is accepted when it is non-empty, contains `{branch}`, keeps all
placeholders within its first label, and — after substituting sample values —
yields a valid DNS name: total ≤253 characters, each label 1–63, characters
limited to `[a-z0-9-]`, no leading or trailing hyphen per label.

`{branch}` is required because without it every branch of a project resolves to
one hostname and each deploy silently displaces the previous. `{service}` is
optional: `{branch}.demo.example.com` is a legitimate single-service pattern.

## Storage

The store's `Data` gains a global section alongside `projects`:

```rust
struct AcmeConfig {
    email: String,
    control_hostname: Option<String>,
    provider: DnsProviderConfig,   // { kind: "cloudflare", token: String }
}
```

The DNS token is a **secret** and follows the registry password's rules exactly:
never returned by any read path, masked in the UI, enforced by a separate masked
type rather than by remembering to omit a field. It is strictly more dangerous
than a registry token — it can rewrite DNS — so it gets the same treatment or
better.

`ProjectData` gains `hostname_template: Option<String>`.

Both are `Option` + `#[serde(default)]`; the on-disk `version` stays at `1` and
existing files load unchanged. Every deletion path must check all three kinds of
project data before pruning a project entry, or removing one silently discards
another.

## Dashboard

A new **TLS & DNS** section, outside the per-project panels:

- ACME account email, and the control hostname.
- DNS provider and API token (masked; set/replace only, never displayed).
- A per-domain certificate table: domain, state (valid until / issuing /
  failed), and the last error when failed. This is what keeps the chosen
  failure mode honest — a permanent silent downgrade to HTTP has to be visible.

Each project panel gains a **Domain** row showing its effective template — its
own, or the global default marked as inherited — with forms to set and reset.

## API

Mirroring the existing project endpoints, under the same bearer-token gate:

- `PUT` / `DELETE /projects/{project}/domain` — body `{"hostname_template": "..."}`.
- `PUT /acme/config` — body `{"email", "control_hostname"}`.
- `PUT` / `DELETE /acme/dns` — body `{"kind": "cloudflare", "token": "..."}`.
- `GET /acme/status` — per-domain certificate state, never including the token.

## Error handling

- **No DNS credential configured:** no issuance is attempted; the dashboard
  shows a setup prompt. Serving is unaffected.
- **Issuance fails:** recorded per domain with its error, backed off, retried.
  The domain keeps serving HTTP.
- **DNS write fails:** the order is abandoned and challenge records cleaned up;
  treated as an issuance failure.
- **Propagation times out:** same, after the configured timeout.
- **Certificate on disk is unparseable:** logged and treated as absent, so a
  corrupt file triggers reissuance rather than a crash loop.
- **Docker unreachable:** unchanged from today — routing and TLS are unaffected.

## Testing

- `DnsProvider`: a fake implementation records calls; Cloudflare's client is
  tested against a mock HTTP server for request shape, fully-qualified naming,
  and preservation of other TXT values at the same name.
- Wildcard-base derivation from templates, including rejection of
  multi-label placeholder patterns.
- Template validation: acceptance and each rejection reason.
- SNI resolution: exact match, wildcard match, one-label-only matching (that
  `a.b.dev.example.com` does **not** match `*.dev.example.com`), and absence.
- Cert store: expiry parsing, due-for-renewal selection, atomic write, `0600`,
  and unparseable-file handling.
- Renewal loop: backoff schedule, and that a domain in backoff is skipped.
- ACME flow against a fake provider and a stubbed ACME server — no live calls
  in the test suite.
- Engine: two projects with different templates produce different hostnames; a
  running container keeps its hostname after its project's template changes.
- API and dashboard: set/read/delete for each endpoint, token masking, and
  route-ordering regressions.
- The token never appears in any response body or rendered HTML.

## Migration

`HOSTER_HTTPS_LISTEN` is unset by default, so upgrading changes nothing. The
operator runs hoster on `:8443` alongside nginx, configures DNS credentials in
the dashboard, watches certificates issue, verifies branches serve over HTTPS,
then moves hoster to `:443` and stops nginx. Reversible at each step.

Binding ports below 1024 requires `AmbientCapabilities=CAP_NET_BIND_SERVICE` in
the systemd unit — one line, documented, and included in the installer.

## Implementation phases

Each phase ends somewhere testable:

1. Template validation and per-project storage.
2. Engine resolution and the URL-loop consolidation.
3. Project domain API and dashboard.
4. `DnsProvider` trait and the Cloudflare implementation.
5. Cert store.
6. ACME issuance, including propagation waiting.
7. TLS listener and SNI resolution.
8. Renewal loop with backoff.
9. TLS & DNS dashboard section and its API.
10. Documentation and the systemd capability change.

Phases 1–3 deliver multi-domain routing on their own and are useful before any
TLS work lands.

## Accepted risks

1. **DNS token at rest.** Stored in `projects.json` under `0600`, unencrypted,
   like the registry password. A DNS-editing token is broader than a registry
   token; an operator should scope it to `Zone:DNS:Edit` on the specific zones.
2. **Rate limits are shared.** The 50-per-registered-domain weekly limit counts
   all issuance for an apex domain regardless of account. An operator running
   several hoster instances against subdomains of one apex shares that budget.
3. **Cloudflare only.** Other providers require implementing the trait. Hetzner's
   DNS moved to the Cloud API in May 2026; Namecheap works with a static IPv4,
   BasicDNS, and API access enabled, but its `setHosts` replaces the entire
   record set, so its implementation must read-modify-write and cannot see
   email-forwarding or redirect records — a deliberate, separately-tested piece
   of work.
