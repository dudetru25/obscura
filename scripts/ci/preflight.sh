#!/usr/bin/env bash
# Run from any directory with the pinned Python dependencies already installed:
# PYTHON_BIN=/path/to/python CHROME_BIN=/path/to/chrome bash scripts/ci/preflight.sh
# Jenkins sources this file to keep the browser selection for both harnesses.
# Linux reference-font and full rendering acceptance remain in jenkins.sh.
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

bash -n "$preflight_root/scripts/ci/jenkins.sh" \
    "$preflight_root/render-repros/run.sh" \
    "$preflight_root/render-repros/representative-suite/run.sh"
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
