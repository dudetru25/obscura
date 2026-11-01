#!/usr/bin/env bash
# Run hit-list regression and unchanged input guards on the native Jenkins worker.
set -euo pipefail
source homelab/jenkins/environment.sh
suite_status=0
for build_mode in default render; do
    feature_args=()
    if [[ $build_mode == render ]]; then feature_args=(--features render); fi
    cargo nextest run --locked --release --no-default-features "${feature_args[@]}" -p obscura-js \
        --lib --no-fail-fast \
        -E 'test(borrowed_document_hit_testing_uses_the_receiver_realm) | test(elements_from_point_lists_every_hit_box_topmost_first) | test(test_elements_from_point_returns_array)' \
        2>&1 | tee "ci-output/hit-list-$build_mode.log" || suite_status=$?
done
cargo nextest run --locked --release --features render -p obscura-cdp \
    --test integration --no-fail-fast -E 'test(/^input_mouse_event_parity::/)' \
    2>&1 | tee ci-output/hit-list-pointer.log || suite_status=$?
cargo nextest run --locked --release --features paint -p obscura-render \
    --lib --no-fail-fast -E 'test(hit_testing)' \
    2>&1 | tee ci-output/hit-list-paint.log || suite_status=$?
exit "$suite_status"
