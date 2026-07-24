#!/usr/bin/env bash
# Polls `1on1ctl --json status` until `capture_health.<field>` reaches the
# expected value or a timeout elapses. Used by .github/workflows/macos-build.yml's
# device-disconnect/reconnect E2E steps to observe the rebinding FSM's health
# (`app_service::TrackHealth`, mirrored as `control_protocol::TrackHealthDto`)
# recovering from a real CoreAudio device-list change, not just a log line.
#
# The expected value is matched as a raw JSON fragment (e.g. '"Ok"' or
# '"Unavailable"') rather than parsed with a JSON tool, matching the plain
# `grep -q` style the rest of this workflow's E2E steps already use.
set -euo pipefail

CTL="${1:?usage: poll-capture-health.sh <ctl-path> <self_health|remote_health> <expected-json-fragment> <timeout-secs>}"
FIELD="${2:?missing field name}"
EXPECTED="${3:?missing expected JSON fragment}"
TIMEOUT_SECS="${4:?missing timeout}"

deadline=$((SECONDS + TIMEOUT_SECS))
last_status=""
while [ "$SECONDS" -lt "$deadline" ]; do
    last_status="$("$CTL" --json status)"
    if echo "$last_status" | grep -q "\"${FIELD}\":${EXPECTED}"; then
        echo "OK: ${FIELD} reached ${EXPECTED} after ${SECONDS}s"
        echo "$last_status"
        exit 0
    fi
    sleep 2
done

echo "FAIL: ${FIELD} did not reach ${EXPECTED} within ${TIMEOUT_SECS}s"
echo "last status: ${last_status}"
exit 1
