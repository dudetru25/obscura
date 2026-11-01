#!/usr/bin/env bash
# Diagnose placement geometry using an already verified release package.
set -euo pipefail
: "${OBSCURA_CANDIDATE_PACKAGE:?}" "${OBSCURA_CANDIDATE_SHA256:?}" "${OBSCURA_CANDIDATE_SOURCE:?}"
[[ $OBSCURA_CANDIDATE_PACKAGE == /* && -f $OBSCURA_CANDIDATE_PACKAGE ]]
[[ $OBSCURA_CANDIDATE_SHA256 =~ ^[a-f0-9]{64}$ && $OBSCURA_CANDIDATE_SOURCE =~ ^[a-f0-9]{40}$ ]]
mkdir -p ci-output/placement-probe
git rev-parse HEAD > ci-output/placement-probe/harness-revision.txt
package_stage=$(mktemp -d /tmp/obscura-placement.XXXXXXXX)
trap 'rm -r -- "$package_stage"' EXIT
printf 'placement package stage: %s\n' "$package_stage"
printf '%s  %s\n' "$OBSCURA_CANDIDATE_SHA256" "$OBSCURA_CANDIDATE_PACKAGE" | sha256sum --check
tar --extract --gzip --file "$OBSCURA_CANDIDATE_PACKAGE" --directory "$package_stage" --no-same-owner --no-same-permissions
(cd "$package_stage" && sha256sum --check SHA256SUMS)
[[ $(cat "$package_stage/SOURCE_REVISION") == "$OBSCURA_CANDIDATE_SOURCE" ]]
"$package_stage/obscura" --version | tee ci-output/placement-probe/engine-version.txt
cp -- "$package_stage/SHA256SUMS" ci-output/placement-probe/engine-checksums.txt
fixture=$(python3 -c 'from pathlib import Path; print(Path("tests/fixtures/placement-height-repro.html").resolve().as_uri())')
OBSCURA_SHOT_W=1920 OBSCURA_SHOT_H=1080 OBSCURA_RENDER_TIMING=1 \
  timeout --kill-after=5s 45s "$package_stage/obscura" fetch "$fixture" --quiet --timeout 30 \
  --screenshot ci-output/placement-probe/placement.png --eval 'window.__placementRepro' \
  -o ci-output/placement-probe/result.json 2> ci-output/placement-probe/timing.log
python3 - <<'PROBE'
import json
import math
import re
from pathlib import Path

root = Path("ci-output/placement-probe")
report = json.loads((root / "result.json").read_text())
if not isinstance(report, dict) or not isinstance(report.get("evaluation"), dict):
    raise SystemExit("placement_probe_missing_evaluation")
value = report["evaluation"]
print(json.dumps(value, indent=2))
if value.get("complete") is not True or value.get("phase") != "complete" or value.get("errors") != []:
    raise SystemExit("placement_probe_incomplete")
if value.get("viewport") != {"width": 1920, "height": 1080}:
    raise SystemExit("placement_probe_viewport_mismatch")
delta = value.get("rootDelta")
if not isinstance(delta, dict) or set(delta) != {"top", "height", "nestedHeight"} or any(
    type(number) not in (int, float) or not math.isfinite(number) for number in delta.values()
):
    raise SystemExit("placement_probe_invalid_geometry")
timing = (root / "timing.log").read_text()
observations = re.findall(r"cq_passes=(\d+) cq_termination=(\S+)", timing)
if not observations:
    raise SystemExit("placement_probe_missing_layout_telemetry")
summary = {"layouts": len(observations), "passes": {}, "termination": {}}
for passes, termination in observations:
    summary["passes"][passes] = summary["passes"].get(passes, 0) + 1
    summary["termination"][termination] = summary["termination"].get(termination, 0) + 1
(root / "timing-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
image = root / "placement.png"
if not image.is_file() or image.stat().st_size == 0:
    raise SystemExit("placement_probe_missing_capture")
if type(value.get("pass")) is not bool:
    raise SystemExit("placement_probe_invalid_verdict")
if not value["pass"]:
    raise SystemExit("placement_height_changed")
PROBE
