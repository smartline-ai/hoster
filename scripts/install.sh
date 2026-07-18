#!/bin/sh
# hoster installer — download the release binary, install a hardened systemd
# service, and start it. Idempotent: re-running upgrades the binary and restarts
# without touching /etc/hoster/hoster.env.
#
#   sudo sh install.sh                 # install / upgrade to the latest release
#   VERSION=v0.1.0 sudo sh install.sh  # pin a version
#   sudo sh install.sh --uninstall     # remove service + binary, keep config
#   sudo sh install.sh --purge         # also remove /etc/hoster and the user
#
# Or straight from GitHub:
#   curl -fsSL https://raw.githubusercontent.com/smartline-ai/hoster/main/scripts/install.sh | sudo sh
#
# Env overrides: VERSION, HOSTER_REPO, PREFIX, HOSTER_TOKEN, HOSTER_LISTEN,
# HOSTER_API_LISTEN, HOSTER_HOSTNAME_TEMPLATE, HOSTER_REGISTRY.

set -eu

REPO="${HOSTER_REPO:-smartline-ai/hoster}"
PREFIX="${PREFIX:-/usr/local}"
BIN_DIR="${PREFIX}/bin"
CONF_DIR="/etc/hoster"
ENV_FILE="${CONF_DIR}/hoster.env"
UNIT_FILE="/etc/systemd/system/hoster.service"
SERVICE_USER="hoster"
ASSET_ARCH="x86_64-linux-musl"
PURGE=0

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

need_root() {
  [ "$(id -u)" -eq 0 ] || die "run as root (try: sudo sh $0)"
}

fetch() { # fetch URL OUTFILE
  if have curl; then
    curl -fsSL "$1" -o "$2"
  elif have wget; then
    wget -qO "$2" "$1"
  else
    die "need curl or wget to download"
  fi
}

fetch_stdout() { # fetch URL -> stdout
  if have curl; then
    curl -fsSL "$1"
  elif have wget; then
    wget -qO- "$1"
  else
    die "need curl or wget to download"
  fi
}

resolve_version() {
  if [ -n "${VERSION:-}" ]; then
    printf '%s' "$VERSION"
    return
  fi
  tag="$(fetch_stdout "https://api.github.com/repos/${REPO}/releases/latest" \
    | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$tag" ] || die "could not resolve latest release for ${REPO} (set VERSION=vX.Y.Z)"
  printf '%s' "$tag"
}

verify_checksum() { # verify_checksum DIR FILE
  dir="$1"; file="$2"
  expected="$(grep " ${file}\$" "${dir}/SHA256SUMS" | awk '{print $1}' | head -1)"
  [ -n "$expected" ] || die "no checksum for ${file} in SHA256SUMS"
  if have sha256sum; then
    actual="$(sha256sum "${dir}/${file}" | awk '{print $1}')"
  elif have shasum; then
    actual="$(shasum -a 256 "${dir}/${file}" | awk '{print $1}')"
  else
    die "need sha256sum or shasum to verify download"
  fi
  [ "$expected" = "$actual" ] || die "checksum mismatch for ${file}"
  log "checksum verified"
}

gen_token() {
  if have openssl; then
    openssl rand -hex 32
  elif [ -r /dev/urandom ]; then
    LC_ALL=C tr -dc 'a-f0-9' < /dev/urandom | dd bs=64 count=1 2>/dev/null
  else
    die "cannot generate a token; pass HOSTER_TOKEN=..."
  fi
}

install_binary() { # install_binary SRC
  mkdir -p "$BIN_DIR"
  install -m 0755 "$1" "${BIN_DIR}/hoster.new"
  mv -f "${BIN_DIR}/hoster.new" "${BIN_DIR}/hoster"
  log "installed ${BIN_DIR}/hoster"
}

ensure_user() {
  if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    useradd --system --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER" \
      || useradd --system --shell /bin/false "$SERVICE_USER" \
      || die "could not create system user ${SERVICE_USER}"
    log "created system user ${SERVICE_USER}"
  fi
  if getent group docker >/dev/null 2>&1; then
    usermod -aG docker "$SERVICE_USER"
    log "added ${SERVICE_USER} to the docker group"
  else
    warn "no docker group found — hoster needs access to the Docker socket."
    warn "install Docker, then: usermod -aG docker ${SERVICE_USER} && systemctl restart hoster"
  fi
}

ensure_conf() {
  mkdir -p "$CONF_DIR"
  if [ -f "$ENV_FILE" ]; then
    log "keeping existing ${ENV_FILE}"
    return
  fi
  token="${HOSTER_TOKEN:-$(gen_token)}"
  : "${HOSTER_LISTEN:=0.0.0.0:80}"
  : "${HOSTER_API_LISTEN:=127.0.0.1:8081}"
  : "${HOSTER_REGISTRY:=localhost:5000}"
  if [ -z "${HOSTER_HOSTNAME_TEMPLATE:-}" ]; then
    HOSTER_HOSTNAME_TEMPLATE='{service}-{branch}.dev.example.com'
  fi
  (
    umask 077
    cat > "$ENV_FILE" <<EOF
# hoster configuration. See https://github.com/${REPO}/blob/main/docs/deploying.md
# Restart after editing: systemctl restart hoster

# Shared bearer token CI must send to the control API. Keep it secret.
HOSTER_TOKEN=${token}

# Public proxy bind address. 0.0.0.0:80 accepts traffic from outside the host.
HOSTER_LISTEN=${HOSTER_LISTEN}

# Control API bind address. Keep private (localhost or a VPN interface).
HOSTER_API_LISTEN=${HOSTER_API_LISTEN}

# How public hostnames are built from {service} and {branch}.
HOSTER_HOSTNAME_TEMPLATE=${HOSTER_HOSTNAME_TEMPLATE}

# Registry base substituted for {{registry}} in image refs.
HOSTER_REGISTRY=${HOSTER_REGISTRY}
EOF
  )
  chown "${SERVICE_USER}:${SERVICE_USER}" "$ENV_FILE" 2>/dev/null || true
  chmod 0600 "$ENV_FILE"
  log "wrote ${ENV_FILE} (token auto-generated — edit to taste)"
}

install_unit() {
  cat > "$UNIT_FILE" <<EOF
[Unit]
Description=hoster — per-branch reverse proxy and deploy engine
Documentation=https://github.com/${REPO}
After=network-online.target docker.service
Wants=network-online.target docker.service

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_USER}
EnvironmentFile=${ENV_FILE}
ExecStart=${BIN_DIR}/hoster
Restart=always
RestartSec=2
# Allow binding :80 as a non-root service.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=full
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
EOF
  log "wrote ${UNIT_FILE}"
}

print_next_steps() {
  cat <<EOF

hoster ${1} is installed and running.

  Status:   systemctl status hoster
  Logs:     journalctl -u hoster -f
  Config:   ${ENV_FILE}   (edit, then: systemctl restart hoster)

Next steps:
  1. Point wildcard DNS at this host, e.g. *.dev.example.com -> $(hostname -I 2>/dev/null | awk '{print $1}')
  2. Set HOSTER_HOSTNAME_TEMPLATE in ${ENV_FILE} to match.
  3. Put a TLS-terminating reverse proxy (nginx) in front if you need HTTPS.
  4. Verify:  curl -fsS http://127.0.0.1:8081/healthz   # -> ok

Deploy guide: https://github.com/${REPO}/blob/main/docs/deploying.md
EOF
}

do_install() {
  need_root
  have systemctl || die "systemd (systemctl) is required"
  ver="$(resolve_version)"
  log "installing hoster ${ver} from ${REPO}"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  base="https://github.com/${REPO}/releases/download/${ver}"
  tarball="hoster-${ver}-${ASSET_ARCH}.tar.gz"

  log "downloading ${tarball}"
  fetch "${base}/${tarball}" "${tmp}/${tarball}"
  fetch "${base}/SHA256SUMS" "${tmp}/SHA256SUMS"
  verify_checksum "$tmp" "$tarball"

  tar -C "$tmp" -xzf "${tmp}/${tarball}"
  src="${tmp}/hoster-${ver}-${ASSET_ARCH}/hoster"
  [ -f "$src" ] || die "hoster binary not found inside ${tarball}"

  install_binary "$src"
  ensure_user
  ensure_conf
  install_unit
  systemctl daemon-reload
  systemctl enable --now hoster
  print_next_steps "$ver"
}

do_uninstall() {
  need_root
  if have systemctl; then
    systemctl disable --now hoster >/dev/null 2>&1 || true
  fi
  rm -f "$UNIT_FILE"
  if have systemctl; then
    systemctl daemon-reload || true
  fi
  rm -f "${BIN_DIR}/hoster"
  log "removed hoster service and binary"
  if [ "$PURGE" -eq 1 ]; then
    rm -rf "$CONF_DIR"
    userdel "$SERVICE_USER" >/dev/null 2>&1 || true
    log "purged ${CONF_DIR} and user ${SERVICE_USER}"
  else
    log "kept ${CONF_DIR} (run with --purge to remove config + user)"
  fi
}

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

ACTION=install
for arg in "$@"; do
  case "$arg" in
    --uninstall) ACTION=uninstall ;;
    --purge)     ACTION=uninstall; PURGE=1 ;;
    -h|--help)   usage; exit 0 ;;
    *)           die "unknown argument: $arg (try --help)" ;;
  esac
done

case "$ACTION" in
  install)   do_install ;;
  uninstall) do_uninstall ;;
esac
