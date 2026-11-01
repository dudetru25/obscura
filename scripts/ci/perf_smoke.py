#!/usr/bin/env python3
"""Interleaved latency, CPU, peak-RSS, and result-correctness comparison."""

from __future__ import annotations

import argparse
import json
import math
import os
import signal
import statistics
import subprocess
import tempfile
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from threading import Thread


ROUNDS = 5
PROCESS_TIMEOUT_SECONDS = 45
LATENCY_FAILURE_RATIO = 1.20
LATENCY_FAILURE_DELTA_MS = 10.0
RSS_FAILURE_RATIO = 1.15
RSS_FAILURE_DELTA_KIB = 8 * 1024
HTML = b"<!doctype html><html><body><main id='root'></main></body></html>"
SCENARIOS = {
    "dom-build": (
        "(function(){var r=document.getElementById('root');for(var i=0;i<5000;i++){var e=document.createElement('div');e.className='row';e.textContent='item-'+i;r.appendChild(e);}return r.children.length;})()",
        5000,
    ),
    "storage": (
        "(function(){for(var i=0;i<2000;i++)localStorage.setItem('k'+i,'value-'+i);var n=0;for(var j=0;j<2000;j++)n+=localStorage.getItem('k'+j).length;return n;})()",
        18890,
    ),
    "dom-lookup": (
        "(() => {let h=document.createElement('div');h.id='probe';document.body.append(h);let n=0;for(let i=0;i<200000;i++)if(document.getElementById('probe')===h)n++;return n;})()",
        200000,
    ),
    "shadow-build": (
        "(() => {let n=0;for(let i=0;i<1500;i++){let h=document.createElement('div');document.body.append(h);let r=h.attachShadow({mode:'open'});let t=document.createElement('span');t.textContent='row';r.append(t);if(h.shadowRoot===r && r.textContent==='row')n++;}return n;})()",
        1500,
    ),
    "shadow-lookup": (
        "(() => {let h=document.createElement('div');document.body.append(h);let r=h.attachShadow({mode:'open'});let n=0;for(let i=0;i<200000;i++)if(h.shadowRoot===r)n++;return n;})()",
        200000,
    ),
    # Each write is followed by a geometry read, so every sample relayouts after
    # invalidation. 1000 reads plus 200 final values that match the last write.
    "textarea-value-layout": (
        "(() => {let r=document.getElementById('root');r.innerHTML='<textarea rows=2 cols=24></textarea>'.repeat(200);let a=r.querySelectorAll('textarea');let n=0;for(let i=0;i<1000;i++){let t=a[i%200];t.value='v'+i+'\\n'+i*7;if(t.offsetHeight>0)n++;}for(let i=0;i<200;i++)if(a[i].value==='v'+(800+i)+'\\n'+(800+i)*7)n++;return n;})()",
        1200,
    ),
    # Input writes share set_form_value and now perform a textarea node check.
    "input-value-layout": (
        "(() => {let r=document.getElementById('root');r.innerHTML='<input>'.repeat(200);let a=r.querySelectorAll('input');let n=0;for(let i=0;i<1000;i++){let t=a[i%200];t.value='v'+i;if(t.offsetHeight>0)n++;}for(let i=0;i<200;i++)if(a[i].value==='v'+(800+i))n++;return n;})()",
        1200,
    ),
    # A div control on the shared layout path, without form state.
    "div-text-layout": (
        "(() => {let r=document.getElementById('root');r.innerHTML='<div></div>'.repeat(200);let a=r.querySelectorAll('div');let n=0;for(let i=0;i<1000;i++){let d=a[i%200];d.textContent='v'+i+' '+i*7;if(d.offsetHeight>0)n++;}for(let i=0;i<200;i++)if(a[i].textContent==='v'+(800+i)+' '+(800+i)*7)n++;return n;})()",
        1200,
    ),
}


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(HTML)))
        self.end_headers()
        self.wfile.write(HTML)

    def log_message(self, _format: str, *_args: object) -> None:
        return


def run_once(
    binary: Path, url: str, expression: str, expected: int, screenshot: Path | None = None
) -> tuple[float, int, float]:
    with tempfile.NamedTemporaryFile(prefix="obscura-rss-", delete=False) as rss_file:
        rss_path = Path(rss_file.name)
    with tempfile.NamedTemporaryFile(prefix="obscura-stdout-", delete=False) as stdout_file:
        stdout_path = Path(stdout_file.name)
    with tempfile.NamedTemporaryFile(prefix="obscura-stderr-", delete=False) as stderr_file:
        stderr_path = Path(stderr_file.name)
    command = [
        "timeout",
        "--signal=KILL",
        str(PROCESS_TIMEOUT_SECONDS),
        "prlimit",
        "--core=0",
        "--fsize=2097152",
        "--nofile=256",
        "--",
        "/usr/bin/time",
        "-f",
        "%M %U %S",
        "-o",
        str(rss_path),
        str(binary),
        "fetch",
        url,
        "--allow-private-network",
        "--quiet",
        "--timeout",
        "20",
        "--wait",
        "0",
        "--eval",
        expression,
    ]
    if screenshot is not None:
        # The timed sample also paints; fetch then wraps the value in a JSON report.
        command += ["--screenshot", str(screenshot)]
    started = time.perf_counter()
    try:
        with stdout_path.open("wb") as stdout_handle, stderr_path.open("wb") as stderr_handle:
            process = subprocess.Popen(
                command,
                stdout=stdout_handle,
                stderr=stderr_handle,
                start_new_session=True,
            )
            try:
                # A Python timeout wait polls exits in steps up to 50 ms.
                process.wait()
            finally:
                # Remove background descendants left by an untrusted candidate binary.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

        elapsed_ms = (time.perf_counter() - started) * 1000
        if process.returncode != 0:
            raise RuntimeError(f"{binary} exited with status {process.returncode}")
        rss, user_seconds, system_seconds = rss_path.read_text(encoding="ascii").split()
        try:
            result = json.loads(stdout_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise RuntimeError(f"{binary} produced an invalid evaluation result") from error
        if screenshot is not None:
            if not isinstance(result, dict) or "evaluation" not in result:
                raise RuntimeError(f"{binary} produced no screenshot evaluation report")
            if not screenshot.is_file() or screenshot.stat().st_size == 0:
                raise RuntimeError(f"{binary} wrote no screenshot")
            result = result["evaluation"]
        if result != expected:
            raise RuntimeError(
                f"{binary} returned {result!r}; expected {expected!r}"
            )
        return elapsed_ms, int(rss), (float(user_seconds) + float(system_seconds)) * 1000
    finally:
        rss_path.unlink(missing_ok=True)
        stdout_path.unlink(missing_ok=True)
        stderr_path.unlink(missing_ok=True)


def median(values: list[float | int]) -> float:
    return float(statistics.median(values))


def p95(values: list[float | int]) -> float:
    # Nearest rank; with five rounds this is the slowest sample.
    return float(sorted(values)[math.ceil(0.95 * len(values)) - 1])


def add_summary(markdown: str) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as output:
            output.write(markdown)
    else:
        print(markdown)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario", action="append", choices=sorted(SCENARIOS))
    parser.add_argument("--screenshot-dir", type=Path)
    args = parser.parse_args()
    for binary in (args.base, args.candidate):
        if not binary.is_file():
            raise SystemExit(f"missing binary: {binary}")
    scenarios = args.scenario or list(SCENARIOS)
    if args.screenshot_dir is not None:
        args.screenshot_dir.mkdir(parents=True, exist_ok=True)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = Thread(target=server.serve_forever, daemon=True)
    thread.start()
    url = f"http://127.0.0.1:{server.server_port}/"
    samples: dict[str, dict[str, dict[str, list[float | int]]]] = {}
    pair_orders: dict[str, list[list[str]]] = {}
    try:
        for scenario in scenarios:
            expression, expected = SCENARIOS[scenario]
            samples[scenario] = {
                label: {"latency_ms": [], "cpu_ms": [], "rss_kib": []}
                for label in ("base", "candidate")
            }
            pair_orders[scenario] = []
            for round_number in range(ROUNDS):
                order = (
                    (("base", args.base), ("candidate", args.candidate))
                    if round_number % 2 == 0
                    else (("candidate", args.candidate), ("base", args.base))
                )
                pair_orders[scenario].append([label for label, _ in order])
                for label, binary in order:
                    screenshot = (
                        args.screenshot_dir / f"{scenario}-{label}-{round_number}.png"
                        if args.screenshot_dir is not None
                        else None
                    )
                    latency_ms, rss_kib, cpu_ms = run_once(
                        binary, url, expression, expected, screenshot
                    )
                    samples[scenario][label]["latency_ms"].append(latency_ms)
                    samples[scenario][label]["cpu_ms"].append(cpu_ms)
                    samples[scenario][label]["rss_kib"].append(rss_kib)
    finally:
        server.shutdown()
        server.server_close()

    report: dict[str, object] = {
        "rounds": ROUNDS,
        "screenshots": args.screenshot_dir is not None,
        "scenarios": {},
    }
    lines = [
        "## Performance smoke comparison\n\n",
        "Fails above 20% and 10 ms for latency, or above 15% and 8 MiB for RSS.\n\n",
        "| Scenario | Base latency | PR latency | Change | Base RSS | PR RSS | Change |\n",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    ]
    failures = []
    for scenario in scenarios:
        base_latency = median(samples[scenario]["base"]["latency_ms"])
        pr_latency = median(samples[scenario]["candidate"]["latency_ms"])
        base_rss = median(samples[scenario]["base"]["rss_kib"])
        pr_rss = median(samples[scenario]["candidate"]["rss_kib"])
        latency_ratio = pr_latency / base_latency
        rss_ratio = pr_rss / base_rss
        report["scenarios"][scenario] = {
            "base_latency_ms": base_latency,
            "candidate_latency_ms": pr_latency,
            "latency_ratio": latency_ratio,
            "base_rss_kib": base_rss,
            "candidate_rss_kib": pr_rss,
            "rss_ratio": rss_ratio,
            "p50_p95": {
                label: {
                    metric: [median(values), p95(values)]
                    for metric, values in samples[scenario][label].items()
                }
                for label in ("base", "candidate")
            },
            "pair_order": pair_orders[scenario],
            "samples": samples[scenario],
        }
        lines.append(
            f"| {scenario} | {base_latency:.1f} ms | {pr_latency:.1f} ms | "
            f"{(latency_ratio - 1) * 100:+.1f}% | {base_rss / 1024:.1f} MiB | "
            f"{pr_rss / 1024:.1f} MiB | {(rss_ratio - 1) * 100:+.1f}% |\n"
        )
        latency_delta = pr_latency - base_latency
        rss_delta = pr_rss - base_rss
        if (
            latency_ratio > LATENCY_FAILURE_RATIO
            and latency_delta > LATENCY_FAILURE_DELTA_MS
        ):
            failures.append(f"{scenario} latency is {latency_ratio:.2f}x the base")
        if rss_ratio > RSS_FAILURE_RATIO and rss_delta > RSS_FAILURE_DELTA_KIB:
            failures.append(f"{scenario} RSS is {rss_ratio:.2f}x the base")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    add_summary("".join(lines))
    for failure in failures:
        print(f"::error::{failure}")
    if failures:
        raise SystemExit("candidate exceeds the performance regression threshold")


if __name__ == "__main__":
    main()
