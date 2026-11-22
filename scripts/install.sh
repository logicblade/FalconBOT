#!/usr/bin/env bash
set -Eeuo pipefail

readonly SERVICE_NAME="falconbot"
readonly SERVICE_USER="falconbot"
readonly INSTALL_PATH="/usr/local/bin/falconbot"
readonly UPDATE_PATH="/usr/local/sbin/falconbot-update"
readonly CONFIG_DIR="/etc/falconbot"
readonly ENV_FILE="${CONFIG_DIR}/falconbot.env"
readonly SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
readonly DATA_DIR="/var/lib/falconbot"
readonly LOG_DIR="/var/log/falconbot"
readonly BINARY_ASSET="falconbot-linux-x86_64"
readonly RELEASE_URL="https://github.com/logicblade/FalconBOT/releases/latest/download"

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

if [[ "${EUID}" -ne 0 ]]; then
    fail "Run this installer as root (for example, with sudo)."
fi

if [[ $# -ne 0 ]]; then
    fail "Usage: $0"
fi

for command_name in curl sha256sum awk install systemctl useradd getent; do
    command -v "${command_name}" >/dev/null 2>&1 \
        || fail "Required command not found: ${command_name}"
done

systemctl --version >/dev/null 2>&1 || fail "systemd is required."
[[ "$(uname -m)" == "x86_64" ]] || fail "This release supports x86_64 Linux servers only."

temporary_dir="$(mktemp -d)"
cleanup() {
    rm -rf -- "${temporary_dir}"
}
trap cleanup EXIT

download_asset() {
    local asset_name="$1"
    curl --fail --location --silent --show-error --retry 3 \
        "${RELEASE_URL}/${asset_name}" \
        --output "${temporary_dir}/${asset_name}"
}

verify_asset() {
    local asset_name="$1"
    local expected_hash
    local actual_hash
    expected_hash="$(awk -v name="${asset_name}" '$2 == name { print $1 }' \
        "${temporary_dir}/SHA256SUMS")"
    [[ "${expected_hash}" =~ ^[[:xdigit:]]{64}$ ]] \
        || fail "No valid checksum found for ${asset_name}."
    actual_hash="$(sha256sum "${temporary_dir}/${asset_name}" | awk '{ print $1 }')"
    [[ "${actual_hash}" == "${expected_hash}" ]] \
        || fail "Checksum verification failed for ${asset_name}."
}

download_asset "SHA256SUMS"
download_asset "${BINARY_ASSET}"
download_asset "update.sh"
verify_asset "${BINARY_ASSET}"
verify_asset "update.sh"

if ! id "${SERVICE_USER}" >/dev/null 2>&1; then
    useradd --system --user-group --home-dir "${DATA_DIR}" \
        --shell /usr/sbin/nologin "${SERVICE_USER}"
fi

install -d -o root -g root -m 0750 "${CONFIG_DIR}"
install -d -o "${SERVICE_USER}" -g "${SERVICE_USER}" -m 0750 \
    "${DATA_DIR}" "${LOG_DIR}"

if [[ ! -f "${ENV_FILE}" ]]; then
    printf 'Telegram bot token: ' >&2
    IFS= read -r -s BOT_TOKEN
    printf '\n' >&2
    [[ "${BOT_TOKEN}" =~ ^[0-9]{6,}:[A-Za-z0-9_-]{20,}$ ]] \
        || fail "The bot token format is invalid."

    read -r -p "Owner Telegram user ID: " OWNER_ID
    [[ "${OWNER_ID}" =~ ^[1-9][0-9]*$ ]] \
        || fail "OWNER_ID must be a positive numeric Telegram ID."

    environment_tmp="$(mktemp "${CONFIG_DIR}/.falconbot.env.XXXXXX")"
    chmod 0600 "${environment_tmp}"
    printf 'BOT_TOKEN=%s\nOWNER_ID=%s\nBOT_DB_PATH=%s/falconbot.sqlite3\nBOT_LOG_PATH=%s/falconbot.log\nRUST_LOG=info\n' \
        "${BOT_TOKEN}" "${OWNER_ID}" "${DATA_DIR}" "${LOG_DIR}" \
        > "${environment_tmp}"
    mv -f -- "${environment_tmp}" "${ENV_FILE}"
    unset BOT_TOKEN OWNER_ID
fi
chown root:root "${ENV_FILE}"
chmod 0600 "${ENV_FILE}"

install -o root -g root -m 0755 "${temporary_dir}/${BINARY_ASSET}" \
    "${INSTALL_PATH}.new"
mv -f -- "${INSTALL_PATH}.new" "${INSTALL_PATH}"
install -o root -g root -m 0755 "${temporary_dir}/update.sh" "${UPDATE_PATH}.new"
mv -f -- "${UPDATE_PATH}.new" "${UPDATE_PATH}"

service_tmp="$(mktemp /etc/systemd/system/.falconbot.service.XXXXXX)"
cat > "${service_tmp}" <<'UNIT'
[Unit]
Description=FalconBOT Telegram Bot
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=falconbot
Group=falconbot
WorkingDirectory=/var/lib/falconbot
EnvironmentFile=/etc/falconbot/falconbot.env
ExecStart=/usr/local/bin/falconbot
Restart=on-failure
RestartSec=5
UMask=0077
NoNewPrivileges=true
PrivateTmp=true
PrivateDevices=true
ProtectSystem=strict
ProtectHome=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true
RestrictSUIDSGID=true
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
CapabilityBoundingSet=
AmbientCapabilities=
ReadWritePaths=/var/lib/falconbot /var/log/falconbot
SystemCallArchitectures=native

[Install]
WantedBy=multi-user.target
UNIT
chown root:root "${service_tmp}"
chmod 0644 "${service_tmp}"
mv -f -- "${service_tmp}" "${SERVICE_FILE}"

systemctl daemon-reload
systemctl enable "${SERVICE_NAME}.service"
if ! systemctl restart "${SERVICE_NAME}.service" \
    || ! systemctl is-active --quiet "${SERVICE_NAME}.service"; then
    fail "The service did not start. Inspect it with: journalctl -u ${SERVICE_NAME} -n 100"
fi

printf 'FalconBOT installed and running.\n'
printf 'Service: systemctl status %s\n' "${SERVICE_NAME}"
printf 'Logs:    journalctl -u %s -f\n' "${SERVICE_NAME}"
printf 'Update:  sudo %s\n' "${UPDATE_PATH}"
