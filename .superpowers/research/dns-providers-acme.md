# ACME DNS-01 against Cloudflare, Hetzner, Namecheap — research notes

Researched 2026-07-19 for a Rust tool that writes/deletes `_acme-challenge` TXT
records automatically (DNS-01, including wildcards). Every claim below is
sourced; anything I couldn't pin to an authoritative source is flagged
`UNVERIFIED` rather than stated as fact.

---

## 1. Cloudflare

### API access prerequisites
- A Cloudflare account (free plan is sufficient) with the zone's nameservers
  actually delegated to Cloudflare (the zone must be "active" on Cloudflare —
  this is the real prerequisite, more than any account tier). No minimum
  balance, no manual "enable API" toggle, no support ticket needed.
- Create an API Token under My Profile → API Tokens.
  [Create API token](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/)

### TXT record write semantics — incremental, not bulk
- Individual CRUD on the `dns_records` resource:
  `POST /zones/{zone_id}/dns_records` creates **one** record and returns its
  ID; `DELETE /zones/{zone_id}/dns_records/{record_id}` deletes **one**
  record by ID. No read-modify-write of the whole zone is ever required.
  [Create DNS Record API reference](https://developers.cloudflare.com/api/resources/dns/subresources/records/methods/create/)
- Multiple TXT records can coexist at the identical name — Cloudflare treats
  each as a separate record object; combined content across same-name
  same-type records must stay ≤8,192 characters.
  [Cannot add DNS records with the same name](https://developers.cloudflare.com/dns/manage-dns-records/troubleshooting/records-with-same-name/)

### Authentication model
- API Token (Bearer), scopable to `Zone:DNS:Edit` on one or more **specific**
  zones — confirmed narrow scoping is fully supported and is Cloudflare's
  recommended path. The older Global API Key + account email is account-wide
  across all zones/products and is explicitly discouraged now.
  [Create API token docs](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/)

### Rate limits
- Global: 1,200 requests / 5 minutes per user/token (~4 req/s sustained);
  200 req/s per IP. Exceeding it blocks **all** API calls for the next 5
  minutes (HTTP 429). User API token quota: 50 tokens; account token quota:
  500. No DNS-record-specific tighter limit is separately documented.
  [Rate limits](https://developers.cloudflare.com/fundamentals/api/reference/limits/)

### Wildcard quirks
- A wildcard record does **not** cover the zone apex, so a cert covering
  both `dev.example.com` and `*.dev.example.com` needs two separate
  authorizations → two TXT records at the same
  `_acme-challenge.dev.example.com` name. Cloudflare allows this (each is
  its own record object/ID), so both challenge values can be live
  simultaneously.
  [Wildcard DNS records](https://developers.cloudflare.com/dns/manage-dns-records/reference/wildcard-dns-records/)
- Practical implication for a tool managing several subdomains in one zone:
  because deletion is by record ID (not "delete everything named X"), the
  tool must track the ID it created for each challenge and delete only that
  one — a careless "delete all TXT at this name" implementation would still
  be able to clobber a concurrent challenge for a sibling cert.

---

## 2. Hetzner DNS

### API access prerequisites — **the landscape changed this year**
The legacy DNS Console (`dns.hetzner.com`) and its REST API
(`dns.hetzner.com/api/v1/...`) has been **fully shut down**. Beta of the
replacement ended Nov 10, 2025 (no new zones creatable on the old system
after that); remaining zones were auto-migrated in April 2026; the old
console/API went read-only May 20, 2026 and is now gone — `dns.hetzner.com`
redirects to `console.hetzner.com`.
[Shutdown of DNS Console incident notice](https://status.hetzner.com/incident/c2146c42-6dd2-4454-916a-19f07e0e5a44)

Practical upshot: as of today, DNS is managed through the unified **Hetzner
Cloud** account/API (`docs.hetzner.cloud`), with zones living inside a Cloud
"project." Prerequisite = a Hetzner Cloud account, the zone imported into a
project, and a Cloud API token (Security → API tokens, Read or Read&Write).
No minimum balance, no IP allowlist, no manual approval step.
[API token generation](https://docs.hetzner.com/dns-console/dns/general/api-access-token/)

Any implementation guide or Rust example predating ~2025 that targets
`dns.hetzner.com/api/v1/records` is now targeting a dead endpoint —
confirmed by live breakage reports in
[go-acme/lego#2743](https://github.com/go-acme/lego/issues/2743) and
[acme.sh#6554](https://github.com/acmesh-official/acme.sh/issues/6554).

### TXT record write semantics — incremental, atomic per-value
The new Cloud DNS API models records as **RRsets** (name+type), with atomic
action endpoints:
`POST /v1/zones/{zone_id}/rrsets/{name}/{type}/actions/add_records` and the
equivalent `.../actions/remove_records`. These add or remove individual
record *values* from the set without touching other rrsets or unrelated
records — confirmed via a live community example hitting exactly this
endpoint for `_acme-challenge` TXT records:
```
POST https://api.hetzner.cloud/v1/zones/{domain}/rrsets/_acme-challenge{subdomain}/TXT/actions/add_records
Authorization: Bearer {token}
{"ttl":300,"records":[{"value":"\"...\""}]}
```
[Hetzner Community: DNS Validated Let's Encrypt Certificates](https://community.hetzner.com/tutorials/letsencrypt-dns/)

This is *better* than plain per-record CRUD for the wildcard+apex case:
`add_records` appends a value to the set and `remove_records` removes just
that value, so two concurrent challenges sharing a name don't clobber each
other as long as each caller only removes the value it added.

`UNVERIFIED`: I could not render the canonical OpenAPI reference page
(`docs.hetzner.cloud/reference/cloud#zone-rrsets-*`) — it's a JS SPA my
fetch tooling can't execute — so the exact request/response schema (error
codes, idempotency behavior, whether `add_records` on a nonexistent rrset
creates it) is confirmed only via the community example and third-party
Ansible module docs, not Hetzner's own reference text. Verify against that
page directly before finalizing an implementation.

### Authentication model
Bearer token created per Hetzner Cloud **project**, Read or Read&Write
scope — no finer per-zone or per-record-type scoping exists today. A
Read&Write token can touch every zone *and every other resource type*
(servers, volumes, etc.) in that project. The only isolation mechanism the
community has found is to put DNS-only zones in their own dedicated Cloud
project so the ACME tool's token can't reach compute resources — a
project-boundary workaround, not real scoping.
[Cloudron forum: scoped API tokens discussion](https://forum.cloudron.io/topic/12279/dns-delegation-subzones-and-scoped-api-tokens)

### Rate limits
`UNVERIFIED` — I could not obtain a numeric limit for the new Cloud DNS
endpoints. General Hetzner Cloud API responses carry `RateLimit-Limit`/
`Ratelimit-Remaining` headers and return 429 on excess (per a third-party
Terraform-provider "investigating rate limits" guide), but that page is
also JS-rendered and I couldn't extract the actual number. Do not assume a
specific req/s figure without confirming against
`docs.hetzner.cloud/reference/cloud` directly.

### Wildcard quirks
Same underlying DNS constraint as any provider (wildcard doesn't cover the
apex, so two TXT authorizations are needed at the same name) — but
Hetzner's rrset `add_records`/`remove_records` model is a natural fit for
carrying multiple concurrent values under one name.

---

## 3. Namecheap

### API access prerequisites — confirmed, and the dynamic-IP concern is real
- **Eligibility to enable API in production**: your account needs at least
  one of — 20+ domains under the account, $50+ balance, or $50+ spent in
  the last 2 years. (Below that, only the separate sandbox environment at
  `api.sandbox.namecheap.com` works, which is not usable for real certs.)
- **IP allowlisting is mandatory and manual**: "You should whitelist at
  least one IP before your API access will begin to work... only IPv4
  addresses can be used." Done exclusively through the web dashboard
  (Profile → Tools → Namecheap API Access → Whitelisted IPs) — **there is
  no API endpoint to update the whitelist programmatically**, and I found
  no CIDR-range or dynamic-DNS-style whitelist mechanism; it's exact IPv4
  addresses only.
  [Intro to API for Developers](https://www.namecheap.com/support/api/intro/)

**Disqualifying implication**: on a machine with a dynamic/rotating public
IP (residential connections, many cloud providers' ephemeral IPs, anything
behind CGNAT), every IP change breaks the API until a human manually
updates the whitelist in the dashboard. This makes Namecheap's API
effectively unusable for unattended automation from a non-static IP unless
you put a static-IP host/proxy in front of it (e.g. run the ACME client
from a VPS with a fixed IP, or a small relay). This is confirmed by
Namecheap's own docs, not just community reports.

### TXT record write semantics — confirmed: wholesale replace, no single-record API
- `namecheap.domains.dns.setHosts` replaces the **entire** host record set.
  Verbatim from the official doc: *"All host records that are not included
  into the API call will be deleted, so add them in addition to new host
  records."*
  [setHosts API doc](https://www.namecheap.com/support/api/methods/domains-dns/set-hosts/)
- The complete official `domains.dns` method list is only: `setDefault`,
  `setCustom`, `getList`, `getHosts`, `getEmailForwarding`,
  `setEmailForwarding`, `setHosts`. **There is no official add-one-record or
  delete-one-record method.**
  [API Methods list](https://www.namecheap.com/support/api/methods/)
  (I found a third-party AI-agent "skill" wrapper advertising `addHost`/
  `removeHost` convenience methods — these are **not** Namecheap API
  methods; they're client-side helpers that still perform the same
  getHosts→merge→setHosts cycle internally, with the same risks below.)
- **Worse than a simple read-modify-write**: `getHosts` does not return
  every record that exists on the domain. Records managed by Namecheap's
  own subsystems — Email Forwarding (MX/SPF/DKIM), URL/Domain Redirect
  (A/CNAME), and some dashboard-configured third-party integration records
  — are invisible to `getHosts` but **are** deleted by a subsequent
  `setHosts` call, because `setHosts` replaces everything it wasn't told
  about. So even a correctly-implemented get→merge→set cycle can silently
  destroy DNS/email records the client never saw.
- **Concurrency**: no ETag/version/conditional-write parameter is
  documented for `setHosts`. Two concurrent read-modify-write cycles (e.g.
  two certs renewing at once, or a renewal racing a manual DNS edit) is a
  classic lost-update race — whichever call's snapshot was read last wins
  and silently discards the other's change. **Confirmed as reported: this
  is a real architectural risk, not an exaggeration.**

### Authentication model
`apiuser` + `apikey` + `username` + `ClientIp` as query/body parameters —
not a bearer token, not OAuth. The API key is a single account-wide secret
with full read/write over *every* domain and *every* Namecheap product in
the account (registrar transfers, nameserver changes, domain locks, SSL
purchases, etc.) — there is no way to scope a key to "DNS only" or to a
single domain. `ClientIp` must match a whitelisted IP (see above).
[setHosts example request](https://www.namecheap.com/support/api/methods/domains-dns/set-hosts/)

### Rate limits
Commonly cited (and corroborated by several independent secondary sources,
including a Namecheap Python SDK page) as **20 requests/minute, 700/hour,
8000/day**, applied per API key across the whole account.
`UNVERIFIED` at medium confidence: I was blocked from directly fetching
Namecheap's own dedicated rate-limits doc page this session, so this figure
is corroborated but not confirmed against Namecheap's primary text here —
verify before hard-coding a client-side throttle.

### Wildcard quirks
`UNVERIFIED`: In principle, `setHosts`'s flat host-list format should allow
two entries with the same `HostName` (`_acme-challenge`) and
`RecordType=TXT` but different `Address` values in a single call, to carry
both the wildcard and apex challenge values simultaneously. I could not
find Namecheap documentation explicitly confirming duplicate `HostName`
entries are preserved as separate TXT values rather than the later one
overwriting the former in their internal representation. Test against the
sandbox environment before relying on this.

---

## 4. Rust ACME crate ecosystem

| Crate | License | Latest release | Monthly downloads | DNS-01? | Verdict |
|---|---|---|---|---|---|
| **instant-acme** | Apache-2.0 | 0.8.5, Feb 24 2026 | ~282k, used by 91 crates | Yes | Actively maintained, recommended |
| **rustls-acme** | Apache-2.0 OR MIT | 0.15.3, Jun 5 2026 | ~34k | **No** | Actively maintained but disqualified — TLS-ALPN-01/HTTP-01 only |
| **acme-lib** | MIT | 0.9.1, Jan 24 2024 | ~1k, <7 dependents | Conceptually yes | Stale (2+ years, no releases) — do not build new work on it |

- **instant-acme** ([lib.rs](https://lib.rs/crates/instant-acme),
  [GitHub](https://github.com/djc/instant-acme)) — async, pure-Rust RFC 8555
  client by djc (also maintains `rustls`/`quinn`/`h2`), used in production
  at Instant Domain Search. Supports all three challenge types; for DNS-01
  it exposes the challenge token/key-authorization and expects the *caller*
  to publish the TXT record via whatever provider API — it has **no
  built-in DNS provider integrations** (Cloudflare/Hetzner/Namecheap client
  code is 100% your responsibility). It **does** handle: account key
  generation/registration with credential serialize/deserialize (you own
  where the JSON is persisted — no built-in storage backend), order
  creation, polling order readiness (`order.poll_ready`, configurable retry
  policy) and polling the issued certificate (`order.poll_certificate`),
  ACME Renewal Information (ARI, RFC 9773) for CA-informed renewal timing,
  finalize/CSR, revocation, and concurrent-order processing. It does **not**
  include a renewal scheduler/daemon — you build the "check every N hours,
  renew when due" loop yourself (optionally informed by the ARI data it
  gives you). MSRV 1.70.
- **rustls-acme** ([lib.rs](https://lib.rs/crates/rustls-acme)) — explicitly
  states its validation mechanism is "TLS-ALPN-01 OR HTTP-01"; no DNS-01
  path exists, so no wildcard support. Not usable for this project's
  wildcard/DNS-01 requirement regardless of how well maintained it is.
- **acme-lib** ([lib.rs](https://lib.rs/crates/acme-lib)) — synchronous,
  last released Jan 2024, minimal adoption. Same generic RFC 8555
  challenge abstraction as instant-acme in concept, but staleness alone is
  disqualifying for new work.

**Recommendation**: build on `instant-acme` for the ACME protocol layer,
and hand-write the three provider-specific DNS record clients plus a
simple renewal-scheduling loop around it — there is no crate that bundles
provider DNS-01 automation with ACME orchestration for these three
providers.

**Forward-looking, not actionable yet**: Let's Encrypt announced (Feb 18,
2026) a new challenge type, **DNS-PERSIST-01**
([IETF draft](https://datatracker.ietf.org/doc/html/draft-ietf-acme-dns-persist-00),
[Let's Encrypt announcement](https://letsencrypt.org/2026/02/18/dns-persist-01)),
which replaces repeated per-issuance TXT record churn with a single
long-lived `_validation-persist.<domain>` TXT record binding a specific
CA+ACME-account, removing DNS writes from the issuance/renewal critical
path entirely. This would sidestep most of the semantics problems above
(no more incremental-vs-bulk-write question at renewal time), but it's
brand new this year, I found no evidence any Rust crate or the three DNS
providers above support it yet, and its production rollout/stability is
unclear from available sources — worth watching, not something to
architect around today.

---

## 5. Let's Encrypt rate limits

All from the official page, last updated per its own footer June 12, 2025,
content as fetched today:
[Rate Limits — Let's Encrypt](https://letsencrypt.org/docs/rate-limits/)

- **New Orders per Account**: 300 orders / 3 hours (refills 1 per 36s). A
  single order/certificate can cover up to 100 identifiers (DNS names).
- **New Certificates per Registered Domain**: 50 certificates / registered
  domain (or IPv4, or IPv6 /64) / 7 days (refills 1 per 202 min). **This is
  global** — it counts all issuance for that registered domain regardless
  of which ACME account requests it. This is the binding constraint for a
  tool issuing/renewing wildcard certs across several subdomains of one
  apex domain: every cert for `*.a.example.com`, `*.b.example.com`, etc.
  all draw from the same 50/7-day bucket for `example.com`.
- **New Certificates per Exact Set of Identifiers** (duplicate-certificate
  limit): 5 certificates / identical identifier set / 7 days (refills 1 per
  34h). Renewals are exempt from the Registered-Domain limit above but
  **are** subject to this one — don't retry a failed renewal of the exact
  same name-set more than ~5x/week.
- **Authorization Failures per Identifier per Account**: 5 failures /
  identifier / hour (refills 1 per 12 min). DNS-01 failures here are most
  commonly caused by setup mistakes (wrong record name/value, propagation
  not waited out) rather than infrastructure issues.
- **New Registrations per IP**: 10 accounts / 3 hours per IPv4 address; 500
  / 3 hours per IPv6 /48 subnet.
- **Overall per-IP request limits** (load-balancer level, rarely binding):
  `/acme/new-nonce` 20 req/s (burst 10), `/acme/new-account` 5 req/s (burst
  15), `/acme/new-order` 300 req/s (burst 200), `/acme/revoke-cert` 10 req/s
  (burst 100), `/acme/renewal-info` 1000 req/s (burst 100), `/acme/*` 250
  req/s (burst 125), `/directory` 40 req/s (burst 40).

**Practical takeaway**: for several wildcard domains under management, plan
around the 50-certs/registered-domain/7-day bucket and the 5/exact-set/week
duplicate limit, not the much more generous per-second request limits —
those will never be the constraint for a normal renewal cadence (certs are
90 days; even daily renewal attempts across a handful of subdomains stay
well under 50/week per apex domain).

---

## Summary of what would disqualify or force an architecture

1. **Namecheap's IP allowlist is a hard blocker on a dynamic IP** — no
   programmatic way to update it, manual-only, IPv4-exact-match. If the
   issuing machine doesn't have a static IP, Namecheap is unusable without
   adding a static-IP relay.
2. **Namecheap has no single-record write API at all** — `setHosts` is
   confirmed to replace the whole host list, `getHosts` doesn't even see
   everything that exists (email forwarding, redirects), and there's no
   documented conditional-write/versioning to guard against concurrent
   updates. Any client must do get→merge→set and accept both the
   visibility gap and the race window as inherent, unfixable API
   limitations — not implementation bugs to route around.
3. **Cloudflare and Hetzner both support true incremental single-value
   writes** (Cloudflare: per-record CRUD by ID; Hetzner: rrset
   add_records/remove_records), and both natively support multiple TXT
   values under one name — exactly what wildcard+apex simultaneous
   challenges need. Cloudflare additionally supports narrow per-zone token
   scoping (`Zone:DNS:Edit`); Hetzner tokens are only project-scoped (no
   per-zone scoping), so isolate DNS-only zones into their own Cloud
   project if using Hetzner.
4. **Hetzner's DNS product moved this year** — the old
   `dns.hetzner.com`/legacy API is gone (shut down May 20, 2026); target
   only the new Hetzner Cloud API (`docs.hetzner.cloud`) with zones as
   Cloud-project resources and rrset actions. Older tutorials/crates
   targeting the legacy API are broken.
5. **instant-acme is the only actively-maintained Rust ACME crate with
   DNS-01 support**; `rustls-acme` is well-maintained but structurally
   excludes DNS-01/wildcards; `acme-lib` is stale. Whichever crate is
   chosen, provider-specific DNS record clients and a renewal scheduler
   are entirely custom work — no crate provides those for Cloudflare/
   Hetzner/Namecheap.
