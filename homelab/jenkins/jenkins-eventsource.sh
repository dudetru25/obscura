#!/usr/bin/env bash
# Run the EventSource pre-feature negative or the focused EventSource checks on the native Jenkins worker.
set -euo pipefail
mode=${1:-}
if (( $# != 1 )) || [[ $mode != negative && $mode != focused ]]; then
    printf 'Select the EventSource check: negative or focused\n' >&2
    exit 2
fi
bash -n homelab/jenkins/jenkins-eventsource.sh
source homelab/jenkins/environment.sh
rustc_version=$(rustc --version)
printf '%s\n' "$rustc_version"

# Each filter must select exactly the cases its source defines before the run starts.
run_exact() {
    local log=$1 expected=$2
    shift 2
    cargo nextest list --locked --release "$@" --message-format json > "ci-output/$log-list.json" || return
    python3 - "ci-output/$log-list.json" "$expected" <<'SELECTED' || return
import json
import sys
suites = json.load(open(sys.argv[1]))['rust-suites'].values()
selected = [f"{suite['binary-id']} {name}" for suite in suites
            for name, case in suite['testcases'].items() if case['filter-match']['status'] == 'matches']
print(*selected, sep='\n')
if len(selected) != int(sys.argv[2]):
    raise SystemExit(f'Expected {sys.argv[2]} selected tests, found {len(selected)}')
SELECTED
    cargo nextest run --locked --release "$@" --no-fail-fast 2>&1 | tee "ci-output/$log.log"
}

case $mode in
negative)
    # Before the feature, the placeholder fires open without sending the stream request.
    run_exact eventsource-negative 1 -p obscura-cdp --test integration --features render \
        -E 'test(=js_fetch_emits_network_events::event_source_delivers_before_eof_keeps_fetch_live_and_close_cancels)'
    ;;
focused)
    # The stream parser uses str::utf8_chunks.
    if [[ ! $rustc_version =~ ^rustc\ 1\.([0-9]+)\. ]] || (( BASH_REMATCH[1] < 79 )); then
        printf 'EventSource needs Rust 1.79 or later: %s\n' "$rustc_version" >&2
        exit 1
    fi
    suite_status=0
    run_exact eventsource-parser 4 -p obscura-js --lib --features render \
        -E 'test(event_stream_) | test(lossy_utf8)' || suite_status=$?
    cdp_filter='test(js_fetch_emits_network_events::)'
    run_exact eventsource-cdp-default 11 -p obscura-cdp --test integration --no-default-features \
        -E "$cdp_filter" || suite_status=$?
    run_exact eventsource-cdp-render 11 -p obscura-cdp --test integration --features render \
        -E "$cdp_filter" || suite_status=$?
    run_exact eventsource-cdp-stealth 17 -p obscura-cdp --test integration --features render,stealth \
        -E "$cdp_filter" || suite_status=$?
    exit "$suite_status"
    ;;
esac
