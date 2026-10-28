#!/usr/bin/env python3
"""Verify canvas recording and video playback through the release binary's CDP API."""

import argparse
import base64
from contextlib import ExitStack
import io
import json
from pathlib import Path
import subprocess
import signal
import tempfile
import time

from PIL import Image
from playwright.sync_api import sync_playwright


RECORD = """async ({automatic, disabled}) => {
    const canvas = document.createElement('canvas');
    canvas.width = 32; canvas.height = 32;
    if (!automatic) document.body.append(canvas);
    const drawing = canvas.getContext('2d');
    drawing.fillStyle = '#ff0000'; drawing.fillRect(0, 0, 32, 32);
    const stream = canvas.captureStream(automatic ? 20 : 0);
    const track = stream.getVideoTracks()[0];
    track.enabled = !disabled;
    const recorder = new MediaRecorder(stream, {mimeType: 'video/webm;codecs=vp8'});
    const chunks = [];
    recorder.ondataavailable = event => { if (event.data.size) chunks.push(event.data); };
    const started = new Promise(resolve => { recorder.onstart = resolve; });
    const stopped = new Promise((resolve, reject) => {
        recorder.onstop = resolve; recorder.onerror = event => reject(event.error);
    });
    try {
        recorder.start(automatic ? undefined : 100);
        drawing.fillStyle = '#ff0000'; drawing.fillRect(0, 0, 32, 32);
        track.requestFrame();
        await started;
        if (automatic) {
            // Match the detached, one-second, 20 fps recording consumer.
            for (let frame = 0; frame < 25; frame++) {
                drawing.fillStyle = frame < 12 ? '#ff0000' : '#0000ff';
                drawing.fillRect(0, 0, 32, 32);
                await new Promise(resolve => setTimeout(resolve, 40));
            }
        } else {
            for (let frame = 0; frame < 12; frame++) {
                drawing.fillStyle = frame === 0 ? '#ff0000' : '#0000ff';
                drawing.fillRect(0, 0, 32, 32); track.requestFrame();
                await new Promise(resolve => requestAnimationFrame(() => setTimeout(resolve, 80)));
            }
        }
        recorder.stop();
        await stopped;
    } finally {
        if (recorder.state !== 'inactive') recorder.stop();
        stream.getTracks().forEach(item => item.stop());
        canvas.remove();
    }
    const bytes = new Uint8Array(await new Blob(chunks, {type: 'video/webm'}).arrayBuffer());
    return {bytes: btoa(String.fromCharCode(...bytes)), chunks: chunks.length,
        lifecycle: [recorder.state, track.readyState, stream.active]};
}"""

LOAD = """async encoded => {
    const bytes = Uint8Array.from(atob(encoded), character => character.charCodeAt(0));
    const url = URL.createObjectURL(new Blob([bytes], {type: 'video/webm'}));
    const video = document.createElement('video');
    video.controls = true;
    video.style.cssText = 'display:block;width:32px;height:32px';
    document.body.append(video);
    const loaded = new Promise((resolve, reject) => {
        video.onloadeddata = resolve; video.onerror = () => reject(new Error(video.error.message));
    });
    video.src = url;
    await loaded;
    return {width: video.videoWidth, height: video.videoHeight, duration: video.duration,
        ready: video.readyState, url};
}"""


def evaluate_bounded(page, expression, argument, stage):
    return page.evaluate("""async ({expression, argument, stage}) => {
        let timer;
        try {
            return await Promise.race([
                (0, eval)('(' + expression + ')')(argument),
                new Promise((_, reject) => {
                    timer = setTimeout(() => reject(new Error(stage + ' timed out')), 10000);
                })
            ]);
        } finally {
            clearTimeout(timer);
        }
    }""", {'expression': expression, 'argument': argument, 'stage': stage})


def stop_server(process):
    process.terminate()
    process.wait(timeout=10)


def deadline_exceeded(_signum, _frame):
    raise TimeoutError('Media CDP acceptance exceeded its 120-second deadline')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    engine = parser.add_mutually_exclusive_group(required=True)
    engine.add_argument('--obscura-bin', type=Path)
    engine.add_argument('--chrome-bin', type=Path)
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    results = []
    with ExitStack() as cleanup:
        signal.signal(signal.SIGALRM, deadline_exceeded)
        signal.alarm(120)
        cleanup.callback(signal.alarm, 0)
        endpoint = None
        if args.obscura_bin:
            directory = Path(cleanup.enter_context(tempfile.TemporaryDirectory(prefix='obscura-media-')))
            ready_file = directory / 'ready.json'
            log = cleanup.enter_context((args.out / 'server.log').open('w'))
            process = subprocess.Popen([str(args.obscura_bin.resolve(strict=True)), 'serve',
                '--host', '127.0.0.1', '--port', '0', '--workers', '1', '--max-connections', '1',
                '--ready-file', str(ready_file)], stdout=log, stderr=log)
            cleanup.callback(stop_server, process)
            deadline = time.monotonic() + 15
            while not ready_file.exists():
                assert process.poll() is None, 'Obscura exited before CDP readiness; see server.log'
                assert time.monotonic() < deadline, 'Obscura CDP readiness timed out; see server.log'
                time.sleep(0.05)
            ready = json.loads(ready_file.read_text())
            assert ready['pid'] == process.pid and ready['host'] == '127.0.0.1'
            assert type(ready['port']) is int and 0 < ready['port'] <= 65535
            assert ready['websocket_path'] == '/devtools/browser'
            endpoint = f"ws://127.0.0.1:{ready['port']}{ready['websocket_path']}"
        driver = cleanup.enter_context(sync_playwright())
        browser = (driver.chromium.connect_over_cdp(endpoint) if endpoint else
                   driver.chromium.launch(executable_path=str(args.chrome_bin.resolve(strict=True)), headless=True))
        cleanup.callback(browser.close)
        context = browser.new_context(viewport={'width': 640, 'height': 480}, device_scale_factor=1)
        cleanup.callback(context.close)
        page = context.new_page()
        page.set_default_timeout(15000)
        errors = []
        page.on('pageerror', lambda error: errors.append(str(error)))
        for name, automatic, disabled in [('manual-chunks', False, False),
                                          ('detached-timed', True, False),
                                          ('disabled-track', False, True)]:
            page.goto('about:blank')
            page.set_content('<!doctype html><html><body style="margin:0;background:white"></body></html>')
            print(name + ': recording', flush=True)
            recording = evaluate_bounded(page, RECORD, {'automatic': automatic, 'disabled': disabled}, name + ': recording')
            encoded = base64.b64decode(recording['bytes'], validate=True)
            assert encoded.startswith(bytes([0x1a, 0x45, 0xdf, 0xa3]))
            assert bytes([0x1f, 0x43, 0xb6, 0x75]) in encoded, 'Recording has no WebM frame cluster'
            assert recording['lifecycle'] == ['inactive', 'ended', False]
            assert recording['chunks'] >= (1 if automatic else 2), 'Timed recording must emit multiple chunks'
            (args.out / f'{name}.webm').write_bytes(encoded)
            loaded = evaluate_bounded(page, LOAD, recording['bytes'], name + ': load')
            assert (loaded['width'], loaded['height']) == (32, 32)
            assert loaded['ready'] >= 2 and loaded['duration'] > 0
            # Native controls can obscure the pixels on a 32px video. Test paint without the overlay.
            page.evaluate("document.querySelector('video').controls = false")
            print(name + ': first screenshot', flush=True)
            png = page.screenshot(path=str(args.out / f'{name}-first.png'))
            first = Image.open(io.BytesIO(png)).convert('RGB').getpixel((8, 8))
            assert max(first) < 30 if disabled else first[0] > 200 and max(first[1:]) < 50, first
            page.evaluate("""() => {
                const video = document.querySelector('video');
                // Both fixtures record for at least one second. Streaming WebM can report Infinity.
                video.currentTime = 0.75;
            }""")
            page.wait_for_function("!document.querySelector('video').seeking")
            png = page.screenshot(path=str(args.out / f'{name}-last.png'))
            last = Image.open(io.BytesIO(png)).convert('RGB').getpixel((8, 8))
            assert max(last) < 30 if disabled else last[2] > 200 and max(last[:2]) < 50, last
            page.evaluate("""() => {
                const video = document.querySelector('video');
                video.currentTime = 0; video.controls = true; video.focus();
            }""")
            assert page.evaluate("document.activeElement === document.querySelector('video')")
            page.keyboard.press('Space')
            page.wait_for_function("!document.querySelector('video').paused && document.querySelector('video').currentTime > 0")
            page.keyboard.press('Space')
            page.wait_for_function("document.querySelector('video').paused")
            reset = page.evaluate("""url => {
                const video = document.querySelector('video');
                video.removeAttribute('src'); video.load(); URL.revokeObjectURL(url);
                return [video.readyState, video.videoWidth, video.videoHeight, video.currentTime, video.error];
            }""", loaded['url'])
            assert reset == [0, 0, 0, 0, None], reset
            results.append({'case': name, 'bytes': len(encoded), 'chunks': recording['chunks'],
                            'dimensions': [loaded['width'], loaded['height']], 'first': first, 'last': last})
            assert not errors, errors
    # Report success only after the owned browser and server have stopped.
    report = {'media_cdp_acceptance': 'passed', 'cases': results}
    (args.out / 'results.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report))


if __name__ == '__main__':
    main()
