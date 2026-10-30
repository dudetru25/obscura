#!/usr/bin/env bash
# Run the fork's release acceptance within its existing Jenkins budget entry.
set -euo pipefail
bash -n homelab/jenkins/jenkins.sh
source homelab/jenkins/environment.sh
python3 -m venv .ci-python
.ci-python/bin/python -m pip install --only-binary=:all: -r scripts/ci/render-requirements.txt
export PYTHON_BIN="$PWD/.ci-python/bin/python"
source scripts/ci/preflight.sh
"$PYTHON_BIN" render-repros/capture-media-recording.py --chrome-bin "$CHROME_BIN" --out "$PWD/ci-output/media-chrome"
# Keep Chrome's metric-sensitive reference fonts aligned with the embedded faces.
export FONTCONFIG_FILE="$PWD/ci-output/fonts.conf"
python3 - <<'FONTS' | tee ci-output/fonts.log
import os
from pathlib import Path
import subprocess
import xml.etree.ElementTree as ET

assets = Path('crates/obscura-render/assets').resolve(strict=True)
cache = Path('ci-output/font-cache').resolve()
cache.mkdir(exist_ok=True)
config = ET.Element('fontconfig')
ET.SubElement(config, 'cachedir').text = str(cache)
ET.SubElement(config, 'include').text = '/etc/fonts/fonts.conf'
ET.SubElement(config, 'dir').text = str(assets)
rejected = ET.SubElement(ET.SubElement(config, 'selectfont'), 'rejectfont')
for filename in ['dejavu*.ttf', 'noto-color-emoji.ttf']:
    ET.SubElement(rejected, 'glob').text = str(assets / filename)
for generic, family in [('sans-serif', 'Liberation Sans'), ('serif', 'Liberation Serif'),
                        ('monospace', 'Liberation Mono')]:
    alias = ET.SubElement(config, 'alias', binding='strong')
    ET.SubElement(alias, 'family').text = generic
    ET.SubElement(ET.SubElement(alias, 'prefer'), 'family').text = family
ET.ElementTree(config).write(os.environ['FONTCONFIG_FILE'], encoding='utf-8', xml_declaration=True)
for family, filename in [('Liberation Sans', 'liberation-sans.ttf'),
                         ('Liberation Serif', 'liberation-serif.ttf'),
                         ('Liberation Mono', 'liberation-mono.ttf'),
                         ('Arial', 'liberation-sans.ttf'),
                         ('sans-serif', 'liberation-sans.ttf'),
                         ('serif', 'liberation-serif.ttf'),
                         ('monospace', 'liberation-mono.ttf')]:
    expected = (assets / filename).resolve(strict=True)
    selected = Path(subprocess.check_output(['fc-match', '-f', '%{file}', family], text=True)).resolve(strict=True)
    print(f'{family}: {selected}', flush=True)
    if selected != expected:
        raise SystemExit(f'Reference font mismatch for {family}: expected {expected}')
FONTS
cargo build --locked --release -p obscura-cli --bins --features render
release_stage=$(mktemp -d "${TMPDIR:-/tmp}/obscura-release.XXXXXXXX")
trap 'rm -r -- "$release_stage"' EXIT
for binary in obscura obscura-worker; do
    [[ -f $CARGO_TARGET_DIR/release/$binary && -x $CARGO_TARGET_DIR/release/$binary && ! -L $CARGO_TARGET_DIR/release/$binary ]]
    install -m 755 "$CARGO_TARGET_DIR/release/$binary" "$release_stage/$binary"
    readelf --dynamic "$release_stage/$binary" > "ci-output/$binary-libraries.txt"
    if grep -q 'Shared library: \[libvpx' "ci-output/$binary-libraries.txt"; then
        printf 'The release still needs shared libvpx: %s\n' "$binary" >&2
        exit 1
    fi
done
cp LICENSE "$release_stage/LICENSE"
cp /usr/share/doc/libvpx-dev/copyright "$release_stage/COPYRIGHT-libvpx"
printf '%s\n' "$source_revision" > "$release_stage/SOURCE_REVISION"
(
    cd "$release_stage"
    sha256sum obscura obscura-worker LICENSE COPYRIGHT-libvpx SOURCE_REVISION > SHA256SUMS
)
archive="$PWD/ci-output/obscura-$source_revision-x86_64-linux.tar.gz"
tar --create --gzip --file "$archive" --directory "$release_stage" \
    obscura obscura-worker LICENSE COPYRIGHT-libvpx SOURCE_REVISION SHA256SUMS
(cd ci-output && sha256sum "${archive##*/}" > "${archive##*/}.sha256")
mkdir "$release_stage/extracted"
tar --extract --gzip --file "$archive" --directory "$release_stage/extracted" --no-same-owner --no-same-permissions
(cd "$release_stage/extracted" && sha256sum --check SHA256SUMS)
export OBSCURA_BIN="$release_stage/extracted/obscura"
"$OBSCURA_BIN" --version
"$PYTHON_BIN" render-repros/capture-media-recording.py --obscura-bin "$OBSCURA_BIN" --out "$PWD/ci-output/media-obscura"
cargo nextest run --locked --release --features render     -E 'test(media::tests::) | binary(media_capture) | test(resize_observer) | test(response_body_exposes_stream) | test(a_blob_url_is_chrome_shaped) | test(changing_iframe_src_releases_the_previous_realm) | test(cancelling_frame_script_fetch_keeps_all_siblings_resumable) | test(detaching_a_parent_discards_its_queued_descendant_work) | test(read_body_capped_) | test(textarea) | binary(textarea_semantics) | test(closed_dialogs_leave_normal_flow_until_opened) | test(parser_image_lifecycle_uses_shared_render_resource) | test(iframe_viewport_tracks_its_embedding_content_box) | test(retired_document_state_follows_javascript_context_lifetime) | test(retained_style_mirror_can_be_removed_after_its_frame_is_released)'     --no-fail-fast 2>&1 | tee ci-output/focused.log
# Exercise the intermittent socket-close boundary repeatedly on the native worker.
cargo nextest run --locked --release --features render -p obscura-js \
    -E 'test(read_body_capped_)' --stress-count 200 --no-fail-fast \
    2>&1 | tee ci-output/body-cap-stress.log
suite_status=0
# Renderer fixtures need paint; check its layout-only library separately.
cargo nextest run --locked --release --workspace --exclude obscura-render --no-fail-fast \
    2>&1 | tee ci-output/full-default.log || suite_status=$?
cargo check --locked --release -p obscura-render --no-default-features \
    2>&1 | tee ci-output/render-layout-check.log || suite_status=$?
cargo nextest run --locked --release --features render --no-fail-fast     2>&1 | tee ci-output/full-render.log || suite_status=$?
if (( suite_status != 0 )); then exit "$suite_status"; fi
python3 "$benchmark/obstacle-course/run.py" --json --runs 1 --warmup 0 > ci-output/obstacle.json
python3 - <<'CHECK'
import json
from pathlib import Path
results = json.loads(Path('ci-output/obstacle.json').read_text())['results']
assert len(results) == 33 and all(item['pass'] is True for item in results), 'obstacle course requires 33/33'
print('Obstacle course: 33/33')
CHECK
bash render-repros/run.sh "$PWD/ci-output/fixtures"
bash render-repros/representative-suite/run.sh "$PWD/ci-output/top"
bash render-repros/representative-suite/run.sh "$PWD/ci-output/bottom" bottom
