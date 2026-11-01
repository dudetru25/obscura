#!/usr/bin/env bash
# Compare two retained release packages with interleaved textarea, input and control samples.
# The packages own the engine revisions; this checkout owns only the harness.
set -euo pipefail
: "${OBSCURA_BASE_PACKAGE:?}" "${OBSCURA_BASE_SHA256:?}"
: "${OBSCURA_CANDIDATE_PACKAGE:?}" "${OBSCURA_CANDIDATE_SHA256:?}"
export PYTHONDONTWRITEBYTECODE=1
mkdir -p ci-output/perf
git rev-parse HEAD > ci-output/perf/harness-revision.txt
(cd scripts/ci && python3 -m unittest test_perf_smoke) 2>&1 | tee ci-output/perf/unittest.log
python3 -m venv .ci-python
.ci-python/bin/python -m pip install --only-binary=:all: -r scripts/ci/render-requirements.txt
package_stage=$(mktemp -d "${TMPDIR:-/tmp}/obscura-perf.XXXXXXXX")
trap 'rm -r -- "$package_stage"' EXIT
for label in base candidate; do
    package_var="OBSCURA_${label^^}_PACKAGE" sha_var="OBSCURA_${label^^}_SHA256"
    package=${!package_var} expected_sha=${!sha_var}
    revision=${package##*/obscura-}
    revision=${revision%-x86_64-linux.tar.gz}
    [[ $revision =~ ^[a-f0-9]{40}$ ]]
    printf '%s  %s\n' "$expected_sha" "$package" | sha256sum --check
    mkdir "$package_stage/$label"
    tar --extract --gzip --file "$package" --directory "$package_stage/$label" --no-same-owner --no-same-permissions
    (cd "$package_stage/$label" && sha256sum --check SHA256SUMS)
    [[ $(cat "$package_stage/$label/SOURCE_REVISION") == "$revision" ]]
    {
        printf 'package %s\npackage_sha256 %s\n' "$package" "$expected_sha"
        cat "$package_stage/$label/SHA256SUMS"
        "$package_stage/$label/obscura" --version
    } > "ci-output/perf/$label-identity.txt"
done
perf_status=0
python3 scripts/ci/perf_smoke.py \
    --base "$package_stage/base/obscura" --candidate "$package_stage/candidate/obscura" \
    --scenario textarea-value-layout --scenario input-value-layout --scenario div-text-layout \
    --screenshot-dir ci-output/perf/captures --output ci-output/perf/results.json \
    2>&1 | tee ci-output/perf/perf.log || perf_status=$?
# Apply the paired-corpus contentless rule to every capture of each engine.
.ci-python/bin/python -I - <<'CAPTURES' 2>&1 | tee ci-output/perf/captures.log || perf_status=$?
import json, sys
from pathlib import Path
import numpy as np
from PIL import Image
sys.path.insert(0, str(Path("render-repros").resolve()))
from check import structural_edges
report = {}
for path in sorted(Path("ci-output/perf/captures").glob("*.png")):
    array = np.asarray(Image.open(path).convert("RGB"))
    rgb = array.astype(np.float32)
    luminance = rgb[:, :, 0] * 0.2126 + rgb[:, :, 1] * 0.7152 + rgb[:, :, 2] * 0.0722
    edges = int(structural_edges(array).sum())
    stddev = round(float(luminance.std()), 6)
    report[path.name] = {
        "size": list(array.shape[1::-1]),
        "structural_edge_pixels": edges,
        "luminance_stddev": stddev,
        "contentless": edges == 0 and stddev <= 0.5,
    }
Path("ci-output/perf/captures.json").write_text(json.dumps(report, indent=2) + "\n")
contentless = [name for name, entry in report.items() if entry["contentless"]]
print(f"{len(report)} captures, {len(contentless)} contentless")
if len(report) != 30 or contentless:
    raise SystemExit(f"expected 30 captures with content; contentless: {contentless}")
CAPTURES
exit "$perf_status"
