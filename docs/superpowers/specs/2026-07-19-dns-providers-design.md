# DNS providers & automatic wildcard A records — design

Date: 2026-07-19
Status: approved (design)
Scope: DNS automation only. The pluggable reverse-proxy (standalone vs. nginx
backend) is a **separate** subsystem with its own spec and is out of scope here.

## Problem

Today hoster automates exactly one piece of DNS: the ACME **DNS-01 TXT
challenge** used to issue wildcard certificates. The `DnsProvider` trait
(`src/dns.rs`) exposes only `upsert_txt` / `delete_txt`, has a single
implementation (Cloudflare) plus a test fake, and is configured by one
**global** `DnsProviderConfig { kind, token }` wired in `main.rs`.

Two gaps:

1. **No A-record automation.** For a branch hostname like
   `web-feature-x.dev.example.com` to resolve, the operator must set up
   wildcard DNS **by hand**. hoster never creates the record.
2. **One provider, Cloudflare only, global.** Different projects can live in
   different zones under different registrars, and the only supported provider
   is Cloudflare.

## Goals

- Support four DNS backends: **Cloudflare**, **Hetzner DNS**, **Namecheap**,
  and **Manual** (no-op fallback — operator manages DNS themselves).
- Automatically ensure a **single wildcard `A` record per project base
  domain**, pointing at the box's public IP.
- Make the DNS provider **per-project with a global default fallback**, mirroring
  how hostname templates already resolve.
- Drive **both** A-record management and the ACME DNS-01 challenge from the same
  resolved per-project provider.
- **Guide the operator through setup in the UI** — per-provider instructions,
  the exact records to create in manual mode, the Namecheap IP-allowlist
  precondition, and the public-IP requirement — so DNS setup succeeds on the
  first try instead of failing silently.

## Non-goals

- Per-branch A records. We deliberately use one wildcard per project base
  (decided during brainstorming).
- Per-project or per-box target IPs. One global `HOSTER_PUBLIC_IP` (single box).
- Managing any record type other than the wildcard `A` and the ACME `TXT`.
- The reverse-proxy backend work (separate spec).

## Key existing facts this builds on

- `settings::wildcard_base(template)` already derives `*.dev.example.com` from a
  hostname template. `renewal::wanted_domains()` collects the wildcard base of
  the default template **and every project template** — this is exactly the set
  of base domains that need a wildcard A record. Reuse it; do not recompute.
- hoster already issues **wildcard certificates** for those bases via DNS-01
  (`acme.rs`, `renewal.rs`). The A record and the cert therefore target the
  **same base domains and the same zones** — so one provider resolution serves
  both.
- The renewal issuer currently receives one global `Arc<dyn DnsProvider>`
  (`main.rs:58`, `acme.rs:67`). This becomes per-domain resolution.
- `dns.rs` already has a mock-HTTP-server test harness (`mock_server`) that
  records request method/path/body. New providers reuse it.

## Design

### 1. Trait extension (`src/dns.rs`)

Extend `DnsProvider` with A-record operations alongside the existing TXT ones:

```rust
#[async_trait]
pub trait DnsProvider: Send + Sync {
    async fn upsert_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;
    async fn delete_txt(&self, name: &str, value: &str) -> anyhow::Result<()>;

    /// Ensure a single A record at `name` resolves to `ip`, replacing any
    /// existing A value(s) at that exact name. Unlike TXT (which appends for
    /// the wildcard+parent challenge pair), an A record has one intended value.
    async fn upsert_a(&self, name: &str, ip: &str) -> anyhow::Result<()>;

    /// Remove the A record at `name`. Absent record is not an error.
    async fn delete_a(&self, name: &str) -> anyhow::Result<()>;
}
```

Notes:
- `name` for the wildcard is the literal `*.<base>` (e.g. `*.dev.example.com`).
  Names crossing the trait remain **fully qualified**, as the module doc already
  requires.
- `upsert_a` is **replace**, not append (opposite of `upsert_txt`). A hostname
  has one A value; leaving a stale one would split resolution.
- The `FakeDns` test provider gains an A-record map and `a_value(name)` for
  assertions.

### 2. Providers

**Cloudflare** (extend existing `CloudflareProvider`): add `upsert_a` /
`delete_a` using the same zone-lookup + per-record CRUD it already uses for TXT.
`upsert_a` finds an existing `type=A` record at the name and PATCHes it, else
POSTs; `delete_a` deletes by id if present.

**Hetzner DNS** (new `HetznerProvider`): token via `Authorization: Bearer`
against `https://dns.hetzner.com/api/v1`. Zone lookup by `GET /zones`
(longest-suffix match, same rule as Cloudflare), records via
`GET/POST/PUT/DELETE /records`. Per-record CRUD, so writes are isolated.
Constructable with an override base URL for the mock tests, exactly like
`CloudflareProvider::with_base_url`.

**Namecheap** (new `NamecheapProvider`): the hard one.
- Auth: `ApiUser`, `ApiKey`, `UserName`, plus a `ClientIp` that **must be
  allowlisted** in the Namecheap account. This is an operational precondition
  we cannot satisfy in code — it must be documented prominently and surfaced in
  a clear error when Namecheap rejects a non-allowlisted IP.
- API shape: `namecheap.domains.dns.getHosts` returns the **entire** host-record
  set; `namecheap.domains.dns.setHosts` **replaces the entire set**. There is no
  per-record endpoint. Therefore every write is **read-merge-write**: getHosts →
  merge our one record (match by name+type, replace value; else append) → setHosts
  with the full set. Dropping a sibling record here is silent data loss, so this
  path gets an explicit preservation test.
- Same read-merge-write applies to the ACME TXT append.

**Manual** (new `ManualProvider`): a no-op implementation. All four methods
return `Ok(())` after `tracing::info!` describing the record the operator must
create themselves (name, type, value/ip). This is the "backup" mode and the
default when no provider is configured.

### 3. Credential model (`src/secrets.rs`)

`DnsProviderConfig { kind: String, token: String }` cannot express Namecheap.
Generalize to a per-provider credential shape:

```rust
pub enum DnsCredentials {
    Token(String),                  // cloudflare, hetzner
    Namecheap { api_user: String, api_key: String, username: String },
    Manual,                         // no credentials
}

pub struct DnsProviderConfig {
    pub kind: String,               // "cloudflare" | "hetzner" | "namecheap" | "manual"
    pub credentials: DnsCredentials,
}
```

- The hand-written `Debug` must redact `token`, `api_key` (and keep redacting on
  any future secret field) — preserve the current redaction guarantee and its
  test (`masked_acme_never_exposes_the_dns_token`). Add a Namecheap analogue.
- The masked/API view (`MaskedAcme`) continues to expose only `kind` +
  `*_set` booleans, never secret material.
- `set_dns_token` today hard-rejects any kind but `cloudflare`. Replace with a
  per-kind validation that accepts the four kinds and the fields each requires,
  with a clear error naming what's missing.

### 4. Per-project config + resolution

- DNS provider config becomes **per-project with a global default**, stored the
  same way project hostname templates and registry creds already are. A project
  with no DNS config falls back to the global default; a global default of
  `manual`/unset means hoster does nothing.
- **Resolution:** for a given base domain (from `wildcard_base(template)`), find
  the project whose (own or default) template produces that base, and use its
  provider config, else the global default. Build a `provider_for(base) ->
  Arc<dyn DnsProvider>` resolver from the store. Factory turns a
  `DnsProviderConfig` into the concrete provider.
- The A-record wiring and the ACME issuer share this one resolver.

### 5. Server IP (`HOSTER_PUBLIC_IP`)

- New global setting from env `HOSTER_PUBLIC_IP` (single box).
- **Required when any non-manual provider is configured.** Validate at config
  time: setting a non-manual provider while `HOSTER_PUBLIC_IP` is unset is a
  loud error, not a silently broken record. `manual` needs no IP.

### 6. Wiring

**A records (deploy path, `engine.rs`):**
- On deploy, compute the project's base via `wildcard_base(template_for(project))`.
  If its resolved provider is non-manual, ensure `*.<base> → HOSTER_PUBLIC_IP`.
- **Idempotent + deduped:** keep an in-memory set of `(base -> ip)` already
  ensured; skip the API call when unchanged, re-ensure when the IP or provider
  config changes. Ensuring is best-effort and must not fail the deploy — a DNS
  error is logged and surfaced, not fatal (the container still runs; resolution
  can be fixed out of band). Rationale: a wildcard is long-lived infrastructure,
  not per-branch state.
- Teardown does **not** delete the wildcard (other branches of the project share
  it). The wildcard's lifetime is the project's, not a branch's.

**ACME (`acme.rs` / `renewal.rs` / `main.rs`):**
- Replace the single global `Arc<dyn DnsProvider>` handed to the issuer with the
  per-domain resolver. `renewal::run_once` iterates `wanted` domains; for each,
  resolve the provider for that base before publishing the challenge.
- Behavior for the default template's base and the ACME control hostname is
  unchanged except that the provider is now resolved (global default when no
  project claims the base).

### 7. UI / API

- Extend the existing TLS & DNS settings surface so the DNS provider can be set
  **per project** and as the **global default**, choosing among the four kinds
  and entering the fields each requires. Secrets remain write-only and masked in
  reads, exactly as the Cloudflare token is today.
- Surface `HOSTER_PUBLIC_IP` state and the validation error when a non-manual
  provider is set without it.

### The UI must *guide* setup, not just collect fields

DNS is the step operators get wrong, and the failure is silent (a record that
saves but never resolves). The UI is responsible for walking the operator
through each provider so setup succeeds the first time:

- **Provider picker with per-kind instructions.** Selecting a kind reveals only
  that kind's fields plus a short, concrete how-to:
  - *Cloudflare* — where to mint a scoped API token (Zone.DNS edit on the zone),
    what scope it needs.
  - *Hetzner* — where to create the DNS API token in the Hetzner DNS console.
  - *Namecheap* — the three fields **and** the two preconditions stated inline
    and unmissable: (1) API access must be enabled on the account, (2) hoster's
    outbound IP **must be added to Namecheap's API allowlist**, shown next to the
    exact `HOSTER_PUBLIC_IP` value so it can be copied straight in.
  - *Manual* — no fields; instead the UI **displays the exact records to create
    yourself** (see below).
- **Show the target IP and its state.** Display the resolved `HOSTER_PUBLIC_IP`
  wherever a provider is configured, and when it is unset with a non-manual
  provider, show the blocking error inline with how to set it — not a generic
  validation failure.
- **Manual mode shows the record to create.** For each project base domain,
  render the literal record the operator must add at their registrar:
  `*.<base>  A  <HOSTER_PUBLIC_IP>` (and, if manual is used while TLS is on, the
  ACME `_acme-challenge.<base>  TXT` note). This turns "backup manual A records"
  into copy-paste, not guesswork.
- **Per-project vs. default is explicit.** The UI makes clear when a project
  inherits the global default provider vs. overrides it, so the operator knows
  which credential a given project's records and certs will use.
- **Verification affordance.** After saving a non-manual provider, offer a
  "check" that confirms the wildcard resolves to `HOSTER_PUBLIC_IP` (reusing the
  resolver hoster already has), so the operator gets a green/red signal instead
  of discovering breakage at first deploy.

## Testing strategy

Reuse the `mock_server` harness in `dns.rs`.

- **Cloudflare A ops:** upsert creates-then-updates (PATCH not duplicate),
  delete removes only the matching A record, both use the fully-qualified
  wildcard name.
- **Hetzner:** zone longest-suffix match; per-record CRUD isolates writes;
  API-level failure surfaces the provider's message and never the token.
- **Namecheap:** read-merge-write **preserves sibling records** (the critical
  data-loss test); a write that only sets our record must round-trip every other
  host back into setHosts; non-allowlisted-IP error is surfaced clearly.
- **Manual:** all ops are no-ops and return `Ok(())`.
- **Resolution:** project override wins over global default; unknown base falls
  back to global default; `manual`/unset yields the no-op provider.
- **Config/secrets:** `Debug` and masked views never expose `token`/`api_key`;
  per-kind validation rejects missing fields; non-manual provider without
  `HOSTER_PUBLIC_IP` errors at config time.
- **Deploy wiring:** deploy ensures the wildcard once per base (deduped);
  teardown does not delete it; a DNS error does not fail the deploy.

## Risks & caveats

- **Namecheap IP allowlist.** hoster's outbound IP must be allowlisted in the
  Namecheap account or every call fails. Unavoidable in code; documented and
  surfaced as a clear error.
- **Namecheap whole-zone replace.** setHosts replaces the entire record set;
  the read-merge-write path must never drop a sibling record. Covered by a
  dedicated preservation test.
- **Coupling ACME to per-project DNS.** Cert issuance now depends on the project's
  provider config being valid. A misconfigured project provider blocks its
  wildcard cert (already subject to renewal backoff, so this degrades rather
  than crashes).
