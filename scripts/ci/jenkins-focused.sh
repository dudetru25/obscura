#!/usr/bin/env bash
# Run media regressions with and without rendering; full acceptance is separate.
set -euo pipefail
source scripts/ci/environment.sh
suite_status=0
for build_mode in default render; do
    feature_args=()
    if [[ $build_mode == render ]]; then feature_args=(--features render); fi
    cargo nextest run --locked --release --no-default-features "${feature_args[@]}" -p obscura-js \
        --lib --test media_capture --no-fail-fast \
        -E 'test(~media::tests::) | binary(=media_capture)' \
        2>&1 | tee "ci-output/focused-$build_mode.log" || suite_status=$?
done
exit "$suite_status"
