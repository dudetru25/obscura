#!/usr/bin/env bash
# Requires Git, GNU timeout, and the pinned Python dependencies.
# Run from any directory:
# PYTHON_BIN=/path/to/python CHROME_BIN=/path/to/chrome bash scripts/ci/preflight.sh
# Source this file to keep the browser selection for both harnesses.
set -euo pipefail

: "${CHROME_BIN:?Set CHROME_BIN to the existing Chrome executable}"
if [[ ! -x "$CHROME_BIN" ]]; then
    echo "Chrome is not executable: $CHROME_BIN" >&2
    exit 2
fi
if [[ -n "${CHROMIUM_BIN:-}" && "${CHROMIUM_BIN:-}" != "$CHROME_BIN" ]]; then
    echo "CHROME_BIN and CHROMIUM_BIN select different browsers" >&2
    exit 2
fi
export CHROMIUM_BIN="$CHROME_BIN"
export PYTHON_BIN="${PYTHON_BIN:-python3}"
export PYTHONDONTWRITEBYTECODE=1
preflight_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Acquire the pinned prerequisite before any compilation or harness tests.
command -v timeout >/dev/null
benchmark="$preflight_root/ci-output/obscura-benchmark"
benchmark_revision=2d08d9a8e860e423b47209434a9fba9c75e6eee6
git init --quiet "$benchmark"
for attempt in 1 2 3; do
    if timeout --kill-after=5s 120s git -C "$benchmark" fetch --quiet --depth=1 \
        https://github.com/h4ckf0r0day/obscura-benchmark.git "$benchmark_revision"; then
        break
    else
        fetch_status=$?
        echo "Benchmark fetch attempt $attempt/3 failed (exit $fetch_status)." >&2
        if (( attempt == 3 )); then exit "$fetch_status"; fi
        sleep 2
    fi
done
git -C "$benchmark" checkout --quiet --detach FETCH_HEAD
if [[ "$(git -C "$benchmark" rev-parse HEAD)" != "$benchmark_revision" ]]; then
    echo "Benchmark revision mismatch. Preflight stopped. Check the pinned fetch." >&2
    exit 1
fi

for script in "$preflight_root/render-repros/run.sh" \
    "$preflight_root/render-repros/representative-suite/run.sh"; do
    bash -n "$script"
done
"$PYTHON_BIN" -m unittest discover -s "$preflight_root/render-repros" -p 'test_*.py'
"$PYTHON_BIN" -m unittest discover -s "$preflight_root/render-repros/representative-suite" -p 'test_*.py'
"$PYTHON_BIN" - <<'CHECK'
import json
import os
import platform
from playwright.sync_api import sync_playwright

versions = {}
with sync_playwright() as playwright:
    for variable in ('CHROME_BIN', 'CHROMIUM_BIN'):
        browser = playwright.chromium.launch(executable_path=os.environ[variable], headless=True)
        page = browser.new_page()
        page.goto('about:blank')
        versions[variable] = browser.version
        browser.close()
if len(set(versions.values())) != 1:
    raise SystemExit('Capture harnesses launched different Chrome versions')
print(json.dumps({'harness_preflight': 'passed', 'platform': platform.system(), 'browsers': versions}))
CHECK
