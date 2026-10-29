#!/usr/bin/env bash
# Run media in both build modes and iframe/ResizeObserver with render.
set -euo pipefail
source scripts/ci/environment.sh
suite_status=0
for build_mode in default render; do
    feature_args=()
    if [[ $build_mode == render ]]; then feature_args=(--features render); fi
    cargo nextest run --locked --release --no-default-features "${feature_args[@]}" -p obscura-js \
        --lib --test media_capture --test iframe_document_transfer --no-fail-fast \
        -E 'test(~media::tests::) | test(~element_style) | test(~resize_observer) | test(bootstrap_source_uses_ascii_snapshot_storage) | binary(=media_capture) | binary(=iframe_document_transfer)' \
        2>&1 | tee "ci-output/focused-$build_mode.log" || suite_status=$?
done
cargo nextest run --locked --release --features render -p obscura-cdp \
    --test input_mouse_event_parity --no-fail-fast \
    2>&1 | tee ci-output/focused-pointer.log || suite_status=$?
exit "$suite_status"
