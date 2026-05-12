#!/usr/bin/env bash
set -Eeuo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "Please run as root, for example: sudo ./scripts/install-ubuntu.sh" >&2
  exit 1
fi

REPO_URL="${CPA_PRO_REPO:-}"
REF="${CPA_PRO_REF:-main}"
INSTALL_ROOT="${CPA_PRO_INSTALL_ROOT:-/opt/cpa-pro}"
APP_DIR="${INSTALL_ROOT}/app"
CONFIG_ROOT="${CPA_PRO_CONFIG_ROOT:-/etc/cpa-pro}"
STATE_DIR="${CPA_PRO_STATE_DIR:-/var/lib/cpa-pro}"
RUN_USER="${CPA_PRO_USER:-cpa-pro}"
GO_VERSION="${GO_VERSION:-1.26.0}"
NODE_MAJOR="${NODE_MAJOR:-22}"
INSTALL_KIRO_CLI="${INSTALL_KIRO_CLI:-1}"
KIRO_DEB_URL="${KIRO_DEB_URL:-https://desktop-release.q.us-east-1.amazonaws.com/latest/kiro-cli.deb}"
PUBLIC_DOMAIN="${CPA_PRO_DOMAIN:-}"
PUBLIC_SCHEME="${CPA_PRO_SCHEME:-http}"
API_PUBLIC_URL="${CPA_PRO_API_URL:-}"
ADMIN_PUBLIC_URL="${CPA_PRO_ADMIN_URL:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --repo)
      REPO_URL="$2"
      shift 2
      ;;
    --ref)
      REF="$2"
      shift 2
      ;;
    --install-root)
      INSTALL_ROOT="$2"
      APP_DIR="${INSTALL_ROOT}/app"
      shift 2
      ;;
    --domain)
      PUBLIC_DOMAIN="$2"
      shift 2
      ;;
    --api-url)
      API_PUBLIC_URL="$2"
      shift 2
      ;;
    --admin-url)
      ADMIN_PUBLIC_URL="$2"
      shift 2
      ;;
    --scheme)
      PUBLIC_SCHEME="$2"
      shift 2
      ;;
    --skip-kiro-cli)
      INSTALL_KIRO_CLI=0
      shift
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

log() {
  printf '\n==> %s\n' "$*"
}

rand_key() {
  local prefix="$1"
  printf '%s%s' "$prefix" "$(openssl rand -hex 24)"
}

normalize_domain() {
  local value="$1"
  value="${value#http://}"
  value="${value#https://}"
  value="${value%%/*}"
  printf '%s' "$value"
}

normalize_public_urls() {
  if [[ "$PUBLIC_DOMAIN" == https://* ]]; then
    PUBLIC_SCHEME="https"
  elif [[ "$PUBLIC_DOMAIN" == http://* ]]; then
    PUBLIC_SCHEME="http"
  fi

  PUBLIC_DOMAIN="$(normalize_domain "$PUBLIC_DOMAIN")"
  API_PUBLIC_URL="${API_PUBLIC_URL%/}"
  ADMIN_PUBLIC_URL="${ADMIN_PUBLIC_URL%/}"

  if [[ -z "$API_PUBLIC_URL" ]]; then
    if [[ -n "$PUBLIC_DOMAIN" ]]; then
      API_PUBLIC_URL="${PUBLIC_SCHEME}://${PUBLIC_DOMAIN}:8317"
    else
      API_PUBLIC_URL="http://SERVER_IP:8317"
    fi
  fi

  if [[ -z "$ADMIN_PUBLIC_URL" ]]; then
    if [[ -n "$PUBLIC_DOMAIN" ]]; then
      ADMIN_PUBLIC_URL="${PUBLIC_SCHEME}://${PUBLIC_DOMAIN}:8990/admin"
    else
      ADMIN_PUBLIC_URL="http://SERVER_IP:8990/admin"
    fi
  elif [[ "$ADMIN_PUBLIC_URL" != */admin ]]; then
    ADMIN_PUBLIC_URL="${ADMIN_PUBLIC_URL}/admin"
  fi
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) echo "amd64" ;;
    aarch64 | arm64) echo "arm64" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
  esac
}

install_base_packages() {
  log "Installing base packages"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y \
    ca-certificates curl git wget unzip xz-utils jq openssl rsync \
    build-essential pkg-config libssl-dev perl systemd
}

install_node() {
  if command -v node >/dev/null 2>&1 && node --version | grep -qE "^v${NODE_MAJOR}\\."; then
    if ! command -v pnpm >/dev/null 2>&1; then
      npm install -g pnpm@9
    fi
    return
  fi

  log "Installing Node.js ${NODE_MAJOR}"
  curl -fsSL "https://deb.nodesource.com/setup_${NODE_MAJOR}.x" | bash -
  apt-get install -y nodejs
  npm install -g pnpm@9
}

install_rust() {
  if command -v cargo >/dev/null 2>&1; then
    return
  fi

  log "Installing Rust"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
}

install_go() {
  if command -v go >/dev/null 2>&1 && go version | grep -q "go${GO_VERSION}"; then
    return
  fi

  log "Installing Go ${GO_VERSION}"
  local arch
  arch="$(detect_arch)"
  local tarball="/tmp/go${GO_VERSION}.linux-${arch}.tar.gz"
  curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-${arch}.tar.gz" -o "$tarball"
  rm -rf /usr/local/go
  tar -C /usr/local -xzf "$tarball"
  ln -sf /usr/local/go/bin/go /usr/local/bin/go
  ln -sf /usr/local/go/bin/gofmt /usr/local/bin/gofmt
}

install_kiro_cli() {
  if [[ "$INSTALL_KIRO_CLI" != "1" ]]; then
    log "Skipping Kiro CLI install"
    return
  fi

  if command -v kiro-cli >/dev/null 2>&1; then
    return
  fi

  log "Installing Kiro CLI"
  local deb="/tmp/kiro-cli.deb"
  wget -O "$deb" "$KIRO_DEB_URL"
  dpkg -i "$deb" || apt-get install -f -y
  command -v kiro-cli >/dev/null 2>&1 || {
    echo "Kiro CLI install finished, but kiro-cli was not found in PATH." >&2
    exit 1
  }
}

prepare_user_and_dirs() {
  log "Preparing service user and directories"
  if ! id "$RUN_USER" >/dev/null 2>&1; then
    useradd --system --create-home --home-dir "$STATE_DIR" --shell /usr/sbin/nologin "$RUN_USER"
  fi

  install -d -m 0755 "$INSTALL_ROOT"
  install -d -m 0750 -o "$RUN_USER" -g "$RUN_USER" "$STATE_DIR"
  install -d -m 0750 -o "$RUN_USER" -g "$RUN_USER" "$STATE_DIR/.local/share"
  install -d -m 0750 "$CONFIG_ROOT"
  install -d -m 0750 "$CONFIG_ROOT/kiro-rs" "$CONFIG_ROOT/cli-proxy-api"
}

sync_source() {
  log "Syncing source"
  local script_dir source_dir tmp_dir
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  source_dir="$(cd "$script_dir/.." && pwd)"

  if [[ ! -d "$source_dir/kiro-rs" || ! -d "$source_dir/cli-proxy-api" ]]; then
    if [[ -z "$REPO_URL" ]]; then
      echo "CPA_PRO_REPO or --repo is required when running installer outside a cloned cpa-pro repo." >&2
      exit 1
    fi
    tmp_dir="$(mktemp -d)"
    git clone --depth 1 --branch "$REF" "$REPO_URL" "$tmp_dir/cpa-pro"
    source_dir="$tmp_dir/cpa-pro"
  fi

  install -d -m 0755 "$APP_DIR"
  rsync -a --delete \
    --exclude '.git/' \
    --exclude 'target/' \
    --exclude 'node_modules/' \
    --exclude 'dist/' \
    --exclude 'config.json' \
    --exclude 'credentials.json' \
    --exclude 'config.yaml' \
    --exclude '.env' \
    "$source_dir/" "$APP_DIR/"
}

load_or_create_secrets() {
  log "Preparing generated keys"
  local secrets_file="$CONFIG_ROOT/secrets.env"
  if [[ -f "$secrets_file" ]]; then
    # shellcheck disable=SC1090
    source "$secrets_file"
  fi

  CPA_API_KEY="${CPA_API_KEY:-$(rand_key sk-cpa-pro-)}"
  CPA_MANAGEMENT_KEY="${CPA_MANAGEMENT_KEY:-$(rand_key sk-cpa-admin-)}"
  KIRO_API_KEY="${KIRO_API_KEY:-$(rand_key sk-kiro-rs-)}"
  KIRO_ADMIN_KEY="${KIRO_ADMIN_KEY:-$(rand_key sk-kiro-admin-)}"

  umask 077
  cat > "$secrets_file" <<EOF
CPA_API_KEY=${CPA_API_KEY}
CPA_MANAGEMENT_KEY=${CPA_MANAGEMENT_KEY}
KIRO_API_KEY=${KIRO_API_KEY}
KIRO_ADMIN_KEY=${KIRO_ADMIN_KEY}
CPA_PRO_GEMINI_OAUTH_CLIENT_ID=${CPA_PRO_GEMINI_OAUTH_CLIENT_ID:-}
CPA_PRO_GEMINI_OAUTH_CLIENT_SECRET=${CPA_PRO_GEMINI_OAUTH_CLIENT_SECRET:-}
CPA_PRO_ANTIGRAVITY_OAUTH_CLIENT_ID=${CPA_PRO_ANTIGRAVITY_OAUTH_CLIENT_ID:-}
CPA_PRO_ANTIGRAVITY_OAUTH_CLIENT_SECRET=${CPA_PRO_ANTIGRAVITY_OAUTH_CLIENT_SECRET:-}
EOF
  chmod 0600 "$secrets_file"
}

write_configs() {
  log "Writing runtime configs"

  cat > "$CONFIG_ROOT/kiro-rs/config.json" <<EOF
{
  "host": "0.0.0.0",
  "port": 8990,
  "apiKey": "${KIRO_API_KEY}",
  "adminApiKey": "${KIRO_ADMIN_KEY}",
  "tlsBackend": "rustls",
  "region": "us-east-1",
  "defaultEndpoint": "ide",
  "loadBalancingMode": "balanced"
}
EOF

  if [[ ! -f "$CONFIG_ROOT/kiro-rs/credentials.json" ]]; then
    printf '[]\n' > "$CONFIG_ROOT/kiro-rs/credentials.json"
  fi

  cat > "$CONFIG_ROOT/cli-proxy-api/config.yaml" <<EOF
host: "0.0.0.0"
port: 8317

remote-management:
  allow-remote: true
  secret-key: "${CPA_MANAGEMENT_KEY}"
  disable-control-panel: false
  panel-github-repository: "https://github.com/router-for-me/Cli-Proxy-API-Management-Center"

auth-dir: "${STATE_DIR}/cli-proxy-api/auths"

api-keys:
  - "${CPA_API_KEY}"

debug: false
logging-to-file: true
usage-statistics-enabled: true
request-retry: 3
max-retry-credentials: 0

routing:
  strategy: "round-robin"

claude-api-key:
  - api-key: "${KIRO_API_KEY}"
    base-url: "http://127.0.0.1:8990"
    models:
      - name: "claude-sonnet-4-20250514"
        alias: "claude-sonnet-4-6"
      - name: "claude-sonnet-4-20250514"
        alias: "claude-sonnet-4-6-thinking"
      - name: "claude-sonnet-4-20250514"
        alias: "kiro-sonnet"
EOF

  chown -R "$RUN_USER:$RUN_USER" "$CONFIG_ROOT" "$STATE_DIR"
  chmod 0600 "$CONFIG_ROOT/kiro-rs/config.json" "$CONFIG_ROOT/kiro-rs/credentials.json" "$CONFIG_ROOT/cli-proxy-api/config.yaml"
}

build_apps() {
  log "Building kiro-rs admin UI"
  cd "$APP_DIR/kiro-rs/admin-ui"
  if command -v corepack >/dev/null 2>&1; then
    corepack enable || true
  fi
  pnpm install --frozen-lockfile
  pnpm build

  log "Building kiro-rs"
  cd "$APP_DIR/kiro-rs"
  # shellcheck disable=SC1091
  [[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
  cargo build --release

  log "Building CLIProxyAPI"
  cd "$APP_DIR/cli-proxy-api"
  local commit build_date
  commit="$(git -C "$APP_DIR" rev-parse --short HEAD 2>/dev/null || echo none)"
  build_date="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  go build -ldflags="-s -w -X 'main.Version=cpa-pro' -X 'main.Commit=${commit}' -X 'main.BuildDate=${build_date}'" \
    -o "$APP_DIR/cli-proxy-api/CLIProxyAPI" ./cmd/server/
}

write_systemd() {
  log "Writing systemd services"

  cat > /etc/systemd/system/cpa-pro-kiro-rs.service <<EOF
[Unit]
Description=cpa-pro kiro-rs Anthropic-compatible Kiro upstream
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${RUN_USER}
Group=${RUN_USER}
WorkingDirectory=${STATE_DIR}/kiro-rs
Environment=HOME=${STATE_DIR}
Environment=XDG_DATA_HOME=${STATE_DIR}/.local/share
Environment=XDG_CONFIG_HOME=${STATE_DIR}/.config
RuntimeDirectory=cpa-pro
ExecStart=${APP_DIR}/kiro-rs/target/release/kiro-rs -c ${CONFIG_ROOT}/kiro-rs/config.json --credentials ${CONFIG_ROOT}/kiro-rs/credentials.json
Restart=always
RestartSec=3
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF

  cat > /etc/systemd/system/cpa-pro-cli-proxy-api.service <<EOF
[Unit]
Description=cpa-pro CLIProxyAPI gateway
After=network-online.target cpa-pro-kiro-rs.service
Wants=network-online.target
Requires=cpa-pro-kiro-rs.service

[Service]
Type=simple
User=${RUN_USER}
Group=${RUN_USER}
WorkingDirectory=${STATE_DIR}/cli-proxy-api
Environment=HOME=${STATE_DIR}
EnvironmentFile=-${CONFIG_ROOT}/secrets.env
ExecStart=${APP_DIR}/cli-proxy-api/CLIProxyAPI -config ${CONFIG_ROOT}/cli-proxy-api/config.yaml
Restart=always
RestartSec=3
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF

  install -d -m 0750 -o "$RUN_USER" -g "$RUN_USER" "$STATE_DIR/kiro-rs" "$STATE_DIR/cli-proxy-api" "$STATE_DIR/cli-proxy-api/auths"
  systemctl daemon-reload
  systemctl enable --now cpa-pro-kiro-rs.service cpa-pro-cli-proxy-api.service
}

open_firewall_if_active() {
  if command -v ufw >/dev/null 2>&1 && ufw status | grep -q "Status: active"; then
    log "Opening UFW ports"
    ufw allow 8317/tcp || true
    ufw allow 8990/tcp || true
  fi
}

print_summary() {
  normalize_public_urls
  log "Install complete"
  cat <<EOF
Services:
  systemctl status cpa-pro-kiro-rs
  systemctl status cpa-pro-cli-proxy-api

Keys:
  ${CONFIG_ROOT}/secrets.env

URLs:
  CLIProxyAPI: ${API_PUBLIC_URL}
  Kiro Admin: ${ADMIN_PUBLIC_URL}

Client:
  ANTHROPIC_BASE_URL=${API_PUBLIC_URL}
  ANTHROPIC_API_KEY=${CPA_API_KEY}

Kiro Admin key:
  ${KIRO_ADMIN_KEY}
EOF
}

main() {
  install_base_packages
  install_node
  install_rust
  install_go
  install_kiro_cli
  prepare_user_and_dirs
  sync_source
  load_or_create_secrets
  write_configs
  build_apps
  write_systemd
  open_firewall_if_active
  print_summary
}

main "$@"
