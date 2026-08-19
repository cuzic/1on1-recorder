#!/usr/bin/env bash
# Polls `1on1ctl --json status` until `capture_health.<field>` reaches the
# expected value or a timeout elapses. Used by .github/workflows/macos-build.yml's
# device-disconnect/reconnect E2E steps to observe the rebinding FSM's health
# (`app_service::TrackHealth`, mirrored as `control_protocol::TrackHealthDto`)
# recovering from a real CoreAudio device-list change, not just a log line.
#
# The expected value is a JSON fragment (e.g. '"Ok"' or '"Unavailable"') compared
# against `jq -c`'s compact serialization of `.capture_health.<field>` — see
# docs/adr/0008-poll-capture-health-fragile-grep-matching.md for why this replaced
# a plain `grep -q "\"${FIELD}\":${EXPECTED}"` substring match: that matched
# anywhere in the whole JSON blob regardless of nesting, so it would have quietly
# started matching an unrelated `"${FIELD}"` key the moment one was ever added
# elsewhere in `StatusDto`. `jq` ships preinstalled on GitHub-hosted macOS runners.
set -euo pipefail

CTL="${1:?usage: poll-capture-health.sh <ctl-path> <self_health|remote_health> <expected-json-fragment> <timeout-secs>}"
FIELD="${2:?missing field name}"
EXPECTED="${3:?missing expected JSON fragment}"
TIMEOUT_SECS="${4:?missing timeout}"

deadline=$((SECONDS + TIMEOUT_SECS))
last_status=""
while [ "$SECONDS" -lt "$deadline" ]; do
    # `set -e` would otherwise abort this whole script silently the moment ctl
    # exits non-zero (e.g. the `desktop` process it talks to has already crashed
    # mid-disconnect-test) — that's a materially different failure than "health
    # never reached the expected value within the timeout" and deserves its own
    # message rather than being indistinguishable from a hung shell.
    if ! last_status="$("$CTL" --json status)"; then
        echo "FAIL: ${CTL} --json status exited non-zero after ${SECONDS}s (is the desktop process still running?)"
        exit 1
    fi
    # Same reasoning as the ctl-failure check above: `set -e` would otherwise abort
    # the whole script silently the moment `ctl`'s output isn't valid JSON (or
    # doesn't have the expected shape), which is exactly the "invisible failure"
    # this script's jq migration was meant to get rid of.
    if ! actual="$(echo "$last_status" | jq -c --arg field "$FIELD" '.capture_health[$field]')"; then
        echo "FAIL: could not parse '\"${FIELD}\"' out of ctl's output as JSON after ${SECONDS}s"
        echo "raw output: ${last_status}"
        exit 1
    fi
    if [ "$actual" = "$EXPECTED" ]; then
        echo "OK: ${FIELD} reached ${EXPECTED} after ${SECONDS}s"
        echo "$last_status"
        exit 0
    fi
    sleep 2
done

echo "FAIL: ${FIELD} did not reach ${EXPECTED} within ${TIMEOUT_SECS}s"
echo "last status: ${last_status}"
exit 1
