#!/usr/bin/env bash
# Run media and iframe regressions without certifying full browser acceptance.
set -euo pipefail
source scripts/ci/environment.sh
cargo nextest run --locked --release --features media -p obscura-js \
    --lib --test media_capture --test iframe_document_transfer --no-fail-fast \
    -E 'test(~media::tests::) | binary(=media_capture) | binary(=iframe_document_transfer)' \
    2>&1 | tee ci-output/focused-regressions.log
