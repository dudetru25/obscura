#!/usr/bin/env bash
# Run the fork's release acceptance within its existing Jenkins budget entry.
set -euo pipefail
export PATH="${HOME}/.cargo/bin:${PATH}"
export CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2
# DEV-3 registry downloads reject multiplexed HTTP/2 streams.
export CARGO_HTTP_MULTIPLEXING=false
export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-${HOME}/.cache}/obscura/target"
export PYTHONDONTWRITEBYTECODE=1
mkdir -p ci-output
cargo --version
cargo nextest --version
pkg-config --modversion vpx
cargo nextest run --locked --release --features media -p obscura-js     -E 'test(media::tests::) | binary(media_capture) | test(resize_observer) | test(response_body_exposes_stream)'     --no-fail-fast 2>&1 | tee ci-output/focused.log
suite_status=0
cargo nextest run --locked --release --features media --no-fail-fast     2>&1 | tee ci-output/full-media.log || suite_status=$?
cargo nextest run --locked --release --features render --no-fail-fast     2>&1 | tee ci-output/full-render.log || suite_status=$?
if (( suite_status != 0 )); then exit "$suite_status"; fi
cargo build --locked --release -p obscura-cli --bins --features media
export OBSCURA_BIN="$CARGO_TARGET_DIR/release/obscura"
"$OBSCURA_BIN" --version
benchmark=ci-output/obscura-benchmark
git init --quiet "$benchmark"
git -C "$benchmark" fetch --quiet --depth=1 https://github.com/h4ckf0r0day/obscura-benchmark.git     2d08d9a8e860e423b47209434a9fba9c75e6eee6
git -C "$benchmark" checkout --quiet --detach FETCH_HEAD
python3 "$benchmark/obstacle-course/run.py" --json --runs 1 --warmup 0 > ci-output/obstacle.json
python3 - <<'CHECK'
import json
from pathlib import Path
results = json.loads(Path('ci-output/obstacle.json').read_text())['results']
assert len(results) == 33 and all(item['pass'] is True for item in results), 'obstacle course requires 33/33'
print('Obstacle course: 33/33')
CHECK
python3 -m venv .ci-python
.ci-python/bin/python -m pip install --only-binary=:all: -r scripts/ci/render-requirements.txt
export PYTHON_BIN="$PWD/.ci-python/bin/python"
: "${CHROME_BIN:?Jenkins must select its existing Chrome reference binary}"
bash render-repros/run.sh "$PWD/ci-output/fixtures"
bash render-repros/representative-suite/run.sh "$PWD/ci-output/top"
bash render-repros/representative-suite/run.sh "$PWD/ci-output/bottom" bottom
