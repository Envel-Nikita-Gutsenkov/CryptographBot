#!/usr/bin/env bash
# Cryptograph deployment & autostart script.
# Usage:
#   ./scripts/start.sh [docker|systemd|bare]
#   Default: docker

set -euo pipefail
cd "$(dirname "$0")/.."

MODE="${1:-docker}"
BIN="./target/release/cryptograph"
SERVICE="cryptograph"
INSTALL_DIR="/opt/cryptograph"

err() { echo "❌ $*" >&2; exit 1; }
ok()  { echo "✅ $*"; }

check_env() {
    [[ -f .env ]] || err ".env not found — copy .env.example and fill in keys"
    grep -q "TELOXIDE_TOKEN=" .env || err "TELOXIDE_TOKEN not set in .env"
    grep -q "CMC_API_KEY=" .env    || err "CMC_API_KEY not set in .env"
    ok "Environment OK"
}

# ── Docker mode ───────────────────────────────────────────────────────────────
deploy_docker() {
    command -v docker >/dev/null || err "docker not installed"
    check_env
    echo "🐳 Building image..."
    docker compose build --pull
    echo "🚀 Starting container..."
    docker compose up -d
    sleep 3
    docker compose ps
    echo ""
    echo "📋 Logs: docker compose logs -f"
    echo "🛑 Stop: docker compose down"
}

# ── Systemd mode ──────────────────────────────────────────────────────────────
deploy_systemd() {
    [[ $EUID -eq 0 ]] || err "systemd deploy requires root (sudo ./scripts/start.sh systemd)"
    check_env
    command -v cargo >/dev/null || err "cargo not installed"

    echo "🔨 Building release binary..."
    cargo build --release --locked

    echo "📦 Installing to ${INSTALL_DIR}..."
    id -u ${SERVICE} &>/dev/null || useradd -r -s /bin/false -d ${INSTALL_DIR} ${SERVICE}
    mkdir -p ${INSTALL_DIR}
    cp ${BIN} ${INSTALL_DIR}/cryptograph
    cp .env ${INSTALL_DIR}/.env
    cp models.json ${INSTALL_DIR}/models.json
    chown -R ${SERVICE}:${SERVICE} ${INSTALL_DIR}
    chmod 600 ${INSTALL_DIR}/.env

    echo "⚙️  Installing systemd unit..."
    cp systemd/cryptograph.service /etc/systemd/system/
    systemctl daemon-reload
    systemctl enable ${SERVICE}
    systemctl restart ${SERVICE}
    sleep 2
    systemctl status ${SERVICE} --no-pager

    ok "Installed! Logs: journalctl -u ${SERVICE} -f"
}

# ── Bare mode (simple loop for testing) ──────────────────────────────────────
deploy_bare() {
    check_env
    command -v cargo >/dev/null || err "cargo not installed"
    echo "🔨 Building..."
    cargo build --release --locked
    ok "Starting bare loop (Ctrl+C to stop)..."
    while true; do
        ${BIN} && CODE=0 || CODE=$?
        echo "⚠️  Process exited (code ${CODE}), restarting in 5s..."
        sleep 5
    done
}

# ── Update (docker) ───────────────────────────────────────────────────────────
update_docker() {
    echo "🔄 Updating..."
    docker compose pull 2>/dev/null || true
    docker compose build
    docker compose up -d
    ok "Updated!"
}

case "${MODE}" in
    docker)   deploy_docker ;;
    systemd)  deploy_systemd ;;
    bare)     deploy_bare ;;
    update)   update_docker ;;
    *)        err "Unknown mode: ${MODE}. Use: docker | systemd | bare | update" ;;
esac
