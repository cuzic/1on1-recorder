#!/usr/bin/env bash
# Asserts the capture-macos smoke test (crates/capture-macos/examples/smoke_test.rs)
# actually wrote a non-trivial amount of raw f32 PCM data — used by
# .github/workflows/macos-build.yml's e2e-best-effort job as its final check.
#
# Deliberately does not assert anything about the *content* being non-silent:
# BlackHole-fed synthetic audio and/or a runner with no real microphone may
# legitimately produce silence while still proving the capture pipeline itself
# (SCStream start -> frame delivery -> file write) ran end to end. A real
# hardware pass (see the project's task list) is what actually confirms
# non-silent, meaningful audio capture.
set -euo pipefail

OUT_FILE="${1:?usage: assert-segment-written.sh <path-to-raw-pcm-file>}"

if [ ! -f "${OUT_FILE}" ]; then
    echo "FAIL: ${OUT_FILE} was not created — capture pipeline did not run to completion."
    exit 1
fi

size_bytes=$(stat -f%z "${OUT_FILE}" 2>/dev/null || stat -c%s "${OUT_FILE}")

# 10 seconds at 16kHz mono f32 = 640,000 bytes; require at least 10% of that so a
# short/partial capture still fails loudly instead of passing on a near-empty file.
MIN_BYTES=64000

if [ "${size_bytes}" -lt "${MIN_BYTES}" ]; then
    echo "FAIL: ${OUT_FILE} is only ${size_bytes} bytes (expected at least ${MIN_BYTES})."
    exit 1
fi

echo "OK: ${OUT_FILE} contains ${size_bytes} bytes of captured PCM data."
