#!/usr/bin/env bash
# Compare two retained release packages with interleaved hit-test and shared-layout control samples.
# The packages own the engine revisions; this checkout owns only the harness.
set -euo pipefail
: "${OBSCURA_BASE_PACKAGE:?}" "${OBSCURA_BASE_SHA256:?}" "${OBSCURA_BASE_SUMS_SHA256:?}"
: "${OBSCURA_CANDIDATE_PACKAGE:?}" "${OBSCURA_CANDIDATE_SHA256:?}" "${OBSCURA_CANDIDATE_SUMS_SHA256:?}"
export PYTHONDONTWRITEBYTECODE=1
mkdir -p ci-output/perf
git rev-parse HEAD > ci-output/perf/harness-revision.txt
(cd scripts/ci && python3 -m unittest test_perf_smoke) 2>&1 | tee ci-output/perf/unittest.log
package_stage=$(mktemp -d "${TMPDIR:-/tmp}/obscura-perf.XXXXXXXX")
trap 'rm -r -- "$package_stage"' EXIT
for label in base candidate; do
    package_var="OBSCURA_${label^^}_PACKAGE" sha_var="OBSCURA_${label^^}_SHA256" sums_var="OBSCURA_${label^^}_SUMS_SHA256"
    package=${!package_var} expected_sha=${!sha_var} expected_sums=${!sums_var}
    revision=${package##*/obscura-}
    revision=${revision%-x86_64-linux.tar.gz}
    [[ $revision =~ ^[a-f0-9]{40}$ ]]
    printf '%s  %s\n' "$expected_sha" "$package" | sha256sum --check
    mkdir "$package_stage/$label"
    tar --extract --gzip --file "$package" --directory "$package_stage/$label" --no-same-owner --no-same-permissions
    # The pinned SHA256SUMS digest pins each inner binary, not only the archive.
    printf '%s  %s\n' "$expected_sums" "$package_stage/$label/SHA256SUMS" | sha256sum --check
    (cd "$package_stage/$label" && sha256sum --check SHA256SUMS)
    [[ $(cat "$package_stage/$label/SOURCE_REVISION") == "$revision" ]]
    version=$("$package_stage/$label/obscura" --version)
    [[ $version =~ ^obscura\ [0-9]+\.[0-9]+\.[0-9]+-dev\+${revision:0:7}$ ]]
    {
        printf 'package %s\npackage_sha256 %s\n' "$package" "$expected_sha"
        cat "$package_stage/$label/SHA256SUMS"
        printf '%s\n' "$version"
    } > "ci-output/perf/$label-identity.txt"
done
# No captures: exact hit order proves the geometry, and painting would hide per-call cost.
python3 scripts/ci/perf_smoke.py \
    --base "$package_stage/base/obscura" --candidate "$package_stage/candidate/obscura" \
    --scenario hit-layout-control --scenario hit-one-1 --scenario hit-one-200 \
    --scenario hit-list-1 --scenario hit-list-200 \
    --output ci-output/perf/results.json \
    2>&1 | tee ci-output/perf/perf.log
