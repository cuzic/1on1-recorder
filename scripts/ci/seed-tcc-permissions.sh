#!/usr/bin/env bash
# Best-effort attempt to pre-grant Screen & System Audio Recording and Microphone
# TCC access for the capture-macos smoke-test binary, so .github/workflows/
# macos-build.yml's e2e-best-effort job can run without a GUI permission prompt
# (which a headless CI runner can never click through).
#
# THIS IS NOT AN APPLE-SUPPORTED MECHANISM. Directly editing TCC.db is an
# undocumented, version-fragile community workaround (see e.g.
# actions/runner-images#9529 and actions/runner-images#7818) — the schema differs
# across macOS releases, and it has been reported to not reliably take effect
# depending on which exact process performs the capture. Failure here is expected
# and handled gracefully by the caller (macos-build.yml's e2e-best-effort job has
# continue-on-error: true precisely because of this). Treat this script as a
# best-effort attempt, not a guarantee.
set -euo pipefail

BINARY_PATH="$(pwd)/target/aarch64-apple-darwin/debug/examples/smoke_test"
TCC_DB="${HOME}/Library/Application Support/com.apple.TCC/TCC.db"

macos_major_version="$(sw_vers -productVersion | cut -d. -f1)"

echo "Seeding TCC.db for macOS ${macos_major_version} at ${TCC_DB}"
echo "Target binary: ${BINARY_PATH}"

if [ ! -f "${TCC_DB}" ]; then
    echo "TCC.db not found at expected path — skipping (this is expected on some runner images)."
    exit 0
fi

# macOS 14+ (Sonoma) added several columns (pid, pid_version, boot_uuid,
# last_reminded) to the access table beyond what earlier releases had. Branch on
# major version rather than guessing a single schema that works everywhere.
if [ "${macos_major_version}" -ge 14 ]; then
    INSERT_COLUMNS="service, client, client_type, auth_value, auth_reason, auth_version, csreq, policy_id, indirect_object_identifier_type, indirect_object_identifier, indirect_object_code_identity, flags, last_modified, pid, pid_version, boot_uuid, last_reminded"
    INSERT_VALUES_TEMPLATE="'%s', '%s', 1, 2, 3, 1, NULL, NULL, NULL, 'UNUSED', NULL, 0, CAST(strftime('%%s','now') AS INTEGER), NULL, NULL, NULL, 0"
else
    INSERT_COLUMNS="service, client, client_type, auth_value, auth_reason, auth_version, csreq, policy_id, indirect_object_identifier_type, indirect_object_identifier, indirect_object_code_identity, flags, last_modified"
    INSERT_VALUES_TEMPLATE="'%s', '%s', 1, 2, 3, 1, NULL, NULL, NULL, 'UNUSED', NULL, 0, CAST(strftime('%%s','now') AS INTEGER)"
fi

seed_one() {
    local service="$1"
    # client_type=1 means "client is a path", matching an ad-hoc/unsigned CI
    # binary identified by its absolute path rather than a signed bundle ID.
    local values
    values=$(printf "${INSERT_VALUES_TEMPLATE}" "${service}" "${BINARY_PATH}")
    sqlite3 "${TCC_DB}" \
        "INSERT OR REPLACE INTO access (${INSERT_COLUMNS}) VALUES (${values});" \
        || echo "Failed to seed ${service} — continuing (see this script's header comment)."
}

seed_one "kTCCServiceScreenCapture"
seed_one "kTCCServiceMicrophone"

echo "TCC.db seeding attempted. Actual effectiveness is only known once the smoke test runs."
