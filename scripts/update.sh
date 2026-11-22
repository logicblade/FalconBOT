#!/usr/bin/env bash
set -Eeuo pipefail

readonly SERVICE_NAME="falconbot"
readonly INSTALL_PATH="/usr/local/bin/falconbot"
readonly BINARY_ASSET="falconbot-linux-x86_64"
readonly RELEASE_URL="https://github.com/logicblade/FalconBOT/releases/latest/download"

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

if [[ "${EUID}" -ne 0 ]]; then
    fail "Run the updater as root (for example, with sudo)."
fi

command -v curl >/dev/null 2>&1 || fail "curl is required."
command -v sha256sum >/dev/null 2>&1 || fail "sha256sum is required."
command -v systemctl >/dev/null 2>&1 || fail "systemd is required."
[[ "$(uname -m)" == "x86_64" ]] || fail "This release supports x86_64 Linux servers only."

[[ -x "${INSTALL_PATH}" ]] || fail "Installed binary not found at ${INSTALL_PATH}."
systemctl cat "${SERVICE_NAME}.service" >/dev/null 2>&1 \
    || fail "The ${SERVICE_NAME}.service unit is not installed."

temporary_dir="$(mktemp -d)"
cleanup() {
    rm -rf -- "${temporary_dir}"
}
trap cleanup EXIT

for asset_name in "SHA256SUMS" "${BINARY_ASSET}"; do
    curl --fail --location --silent --show-error --retry 3 \
        "${RELEASE_URL}/${asset_name}" \
        --output "${temporary_dir}/${asset_name}"
done

expected_hash="$(awk -v name="${BINARY_ASSET}" '$2 == name { print $1 }' \
    "${temporary_dir}/SHA256SUMS")"
[[ "${expected_hash}" =~ ^[[:xdigit:]]{64}$ ]] \
    || fail "No valid checksum found for the release binary."
actual_hash="$(sha256sum "${temporary_dir}/${BINARY_ASSET}" | awk '{ print $1 }')"
[[ "${actual_hash}" == "${expected_hash}" ]] \
    || fail "Release binary checksum verification failed."

if cmp -s "${temporary_dir}/${BINARY_ASSET}" "${INSTALL_PATH}"; then
    printf 'The installed binary already matches the latest release.\n'
    exit 0
fi

install -o root -g root -m 0755 "${temporary_dir}/${BINARY_ASSET}" \
    "${temporary_dir}/falconbot.new"
cp -a -- "${INSTALL_PATH}" "${temporary_dir}/falconbot.previous"
mv -f -- "${temporary_dir}/falconbot.new" "${INSTALL_PATH}"

if ! systemctl restart "${SERVICE_NAME}.service" \
    || ! systemctl is-active --quiet "${SERVICE_NAME}.service"; then
    printf 'Update failed to start; restoring the previous binary.\n' >&2
    install -o root -g root -m 0755 "${temporary_dir}/falconbot.previous" \
        "${INSTALL_PATH}.rollback"
    mv -f -- "${INSTALL_PATH}.rollback" "${INSTALL_PATH}"
    if ! systemctl restart "${SERVICE_NAME}.service"; then
        printf 'Rollback restart also failed. Inspect: journalctl -u %s -n 100\n' \
            "${SERVICE_NAME}" >&2
    fi
    exit 1
fi

printf 'FalconBOT updated and restarted successfully.\n'
systemctl --no-pager --full status "${SERVICE_NAME}.service"
