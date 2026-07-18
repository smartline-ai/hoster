# Release CI/CD + server install — design

Ship hoster as a downloadable release binary, install it on a server with one
command, and document the operator deploy path. Three artifacts plus supporting
CI. Scope is the **hoster binary's own release and install** — not the
app-author flow, which `docs/deploying.md` already covers.

## Decisions

| Question | Choice |
| --- | --- |
| Release trigger | Push a `v*` git tag → GitHub Release with attached binary. |
| Build target | `x86_64-unknown-linux-musl` — one fully static binary, no glibc pitfalls. |
| Install scope | Binary + systemd unit + env file. nginx/TLS/DNS stay documented-manual. |
| Delivery | `scripts/install.sh`, also runnable via `curl -fsSL … | sudo sh`. |
| Token | Install script auto-generates a random `HOSTER_TOKEN` if none supplied. |
| Socket access | `hoster` system user added to the `docker` group. |

## 1. Release pipeline — `.github/workflows/release.yml`

Trigger: `push` on tags matching `v*`. Single job on `ubuntu-latest`.

Steps:
1. Checkout.
2. Guard: assert the tag (`v0.1.0` → `0.1.0`) equals `Cargo.toml` `version`.
   Fail the release if they drift.
3. Install Rust stable + `x86_64-unknown-linux-musl` target + `musl-tools`.
4. `cargo build --release --target x86_64-unknown-linux-musl`.
5. Stage artifacts:
   - `hoster-<tag>-x86_64-linux-musl.tar.gz` (binary + `install.sh` + README).
   - bare `hoster` binary.
   - `SHA256SUMS`.
6. Publish GitHub Release (`softprops/action-gh-release`) with those assets and
   auto-generated notes.

The git tag is the single source of truth for the version.

## 2. Continuous CI — `.github/workflows/ci.yml`

Trigger: pull_request + push to `main`. Jobs:
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo build`
- `cargo test` (Docker integration tests self-skip with no daemon)
- `shellcheck scripts/install.sh`

Keeps `main` releasable and the install script lint-clean.

## 3. Server install script — `scripts/install.sh`

POSIX `sh`, idempotent, root. Env overrides: `VERSION`, `HOSTER_REPO`,
`PREFIX`, `HOSTER_LISTEN`, `HOSTER_API_LISTEN`, etc.

Flow:
1. Resolve version: `$VERSION` or latest release from the GitHub releases API.
2. Download the musl tarball + `SHA256SUMS`; **verify checksum**; extract
   `hoster` → `/usr/local/bin/hoster` (atomic replace).
3. Create `hoster` system user (no login shell) and `/etc/hoster/`.
4. Write `/etc/hoster/hoster.env` **only if absent** — never clobber an existing
   token. Template: `HOSTER_TOKEN` (random if blank), `HOSTER_LISTEN`,
   `HOSTER_API_LISTEN`, `HOSTER_HOSTNAME_TEMPLATE`, `HOSTER_REGISTRY`.
5. Add `hoster` user to `docker` group (socket access).
6. Install `/etc/systemd/system/hoster.service`:
   `EnvironmentFile=/etc/hoster/hoster.env`, `After=docker.service`,
   `Restart=always`, hardening (`NoNewPrivileges`, `ProtectSystem=full`,
   `ProtectHome`).
7. `systemctl daemon-reload`, `enable --now hoster`, print status + next steps
   (wildcard DNS, nginx/TLS → link to docs).

Re-run = upgrade binary + restart, config untouched. `--uninstall` reverses:
stop/disable service, remove unit + binary; leaves `/etc/hoster` unless
`--purge`.

## 4. README.md (new, top-level)

Sections: what hoster is (paragraph + diagram), operator quickstart (`curl |
sudo sh` → set token → wildcard DNS → verify), from-source build, reverse
proxy/TLS note (manual), releasing (maintainers: `git tag vX.Y.Z && git push
--tags`), links to `docs/deploying.md` for the app-author side.

## Testing

- `shellcheck` in CI.
- `systemd-analyze verify` on the generated unit where available.
- Local dry-run of install.sh resolution/checksum logic against a stub.
- The release workflow is proven by cutting the first real `v0.1.0` tag.

## Out of scope

nginx/certbot automation, non-x86_64 targets, multi-node, container-image
packaging. All revisitable later.
