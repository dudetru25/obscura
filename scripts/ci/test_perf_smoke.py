import json
import signal
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import perf_smoke


class ProcessMeasurementTests(unittest.TestCase):
    def test_exit_wait_is_not_polled_and_keeps_limits_and_cleanup(self):
        process = Mock(pid=12345, returncode=0)
        process.wait.return_value = 0
        commands = []

        def start(command, **kwargs):
            commands.append(command)
            kwargs["stdout"].write(b"5000.0\n")
            kwargs["stdout"].flush()
            Path(command[command.index("-o") + 1]).write_text("1024 0.10 0.02\n")
            self.assertTrue(kwargs["start_new_session"])
            return process

        with patch.object(perf_smoke.subprocess, "Popen", side_effect=start), \
                patch.object(perf_smoke.os, "killpg") as cleanup:
            _, rss, cpu_ms, result, stderr = perf_smoke.run_once(Path("/fake/engine"), "http://localhost/", "5000", 5000)
        self.assertEqual(rss, 1024)
        self.assertEqual(result, 5000)
        self.assertEqual(stderr, "")
        self.assertAlmostEqual(cpu_ms, 120.0)
        process.wait.assert_called_once_with()
        self.assertEqual(commands[0][:4], ["timeout", "--signal=KILL", "45", "prlimit"])
        self.assertIn("--fsize=2097152", commands[0])
        self.assertIn("--nofile=256", commands[0])
        cleanup.assert_called_once_with(process.pid, signal.SIGKILL)

    def test_hung_process_is_still_bounded_without_polling(self):
        with tempfile.TemporaryDirectory() as directory:
            engine = Path(directory) / "engine"
            engine.write_text(f"#!{sys.executable}\nimport time\ntime.sleep(3)\nprint(5000)\n")
            engine.chmod(0o755)
            started = time.monotonic()
            with patch.object(perf_smoke, "PROCESS_TIMEOUT_SECONDS", 0.1), \
                    self.assertRaisesRegex(RuntimeError, "timed out|exited with status"):
                perf_smoke.run_once(engine, "http://localhost/", "5000", 5000)
            self.assertLess(time.monotonic() - started, 2.0)

    def test_probe_counts_render_telemetry_and_fails_closed_without_it(self):
        fast = "[timing] parse+index=1ms stylesheet_cache_hit=false cascade=1ms rules=1 id_keys=0 class_keys=0 attr_keys=0 local_keys=0 universal=0 cq_passes=1 cq_termination=no-queries retained_reused=0 retained_fresh=1 retained_fallback=0"
        multi = "[timing] parse+index=1ms stylesheet_cache_hit=true cascade_total=2ms rules=2 id_keys=0 class_keys=0 attr_keys=0 local_keys=0 universal=0 cq_passes=2 cq_termination=GeometryStable cq_evaluations=0 cq_cache_hits=0 cq_ancestor_steps=0 retained_reused=1 retained_fresh=0 retained_fallback=1"
        with tempfile.TemporaryDirectory() as directory:
            engine = Path(directory) / "engine"
            engine.write_text(
                f"#!{sys.executable}\nimport os, sys\nprint(1200.0)\n"
                f"if os.environ.get('OBSCURA_RENDER_TIMING'):\n"
                f"    print({fast!r}, file=sys.stderr)\n    print({multi!r}, file=sys.stderr)\n    print({multi!r}, file=sys.stderr)\n"
            )
            engine.chmod(0o755)
            result, telemetry = perf_smoke.probe_layout(engine, "http://localhost/", "1200", 1200)
            self.assertEqual(result, 1200)
            self.assertEqual(telemetry, {
                "layouts": 3,
                "cq_passes": {"1": 1, "2": 2},
                "cq_termination": {"no-queries": 1, "GeometryStable": 2},
                "retained_fallback": 2,
            })
            engine.write_text(f"#!{sys.executable}\nprint(1200.0)\n")
            with self.assertRaisesRegex(RuntimeError, "no container layout telemetry"):
                perf_smoke.probe_layout(engine, "http://localhost/", "1200", 1200)
            for payload in ("NaN", "Infinity", "-Infinity"):
                engine.write_text(f"#!{sys.executable}\nprint({payload!r})\n")
                with self.subTest(payload=payload), self.assertRaisesRegex(RuntimeError, "non-integer evaluation result"):
                    perf_smoke.run_once(engine, "http://localhost/", "1200", 1200)
            # Every measurement and telemetry probe must return a typed count.
            for result in (None, True, {}, [], 1200.5):
                engine.write_text(f"#!{sys.executable}\nimport json\nprint(json.dumps({result!r}))\n")
                with self.subTest(result=result), self.assertRaisesRegex(RuntimeError, "non-integer evaluation result"):
                    perf_smoke.run_once(engine, "http://localhost/", "1200", 1200)


    def test_new_feature_reports_only_candidate_cost(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base, candidate, output = root / "base", root / "candidate", root / "results.json"
            base.touch()
            candidate.touch()

            two_passes = 1000

            def measure(binary, *arguments):
                self.assertEqual(binary, candidate)
                return 100.0, 8192, 100.0, 1200, ""

            def probe(binary, *arguments):
                self.assertEqual(binary, candidate)
                return 1200, {
                    "layouts": two_passes,
                    "cq_passes": {"2": two_passes},
                    "cq_termination": {"GeometryStable": two_passes},
                    "retained_fallback": 0,
                }

            arguments = ["perf_smoke.py", "--base", str(base), "--candidate", str(candidate),
                         "--output", str(output), "--scenario", "container-units-layout"]
            with patch.object(sys, "argv", arguments), patch.object(perf_smoke, "ROUNDS", 1), \
                    patch.object(perf_smoke, "run_once", side_effect=measure), \
                    patch.object(perf_smoke, "probe_layout", side_effect=probe), \
                    patch.object(perf_smoke, "add_summary") as summary:
                perf_smoke.main()
            report = json.loads(output.read_text())["scenarios"]["container-units-layout"]
            self.assertFalse(report["comparison_valid"])
            self.assertIsNone(report["latency_ratio"])
            self.assertIsNone(report["rss_ratio"])
            self.assertEqual(report["evaluations"], {"candidate": [1200]})
            self.assertEqual(report["pair_order"], [["candidate"]])
            self.assertIsNone(report["base_latency_ms"])
            self.assertIsNone(report["base_rss_kib"])
            self.assertEqual(report["candidate_latency_ms"], 100.0)
            self.assertEqual(set(report["samples"]), {"candidate"})
            self.assertIn("n/a: base unsupported", summary.call_args.args[0])
            two_passes = 999
            with patch.object(sys, "argv", arguments), patch.object(perf_smoke, "ROUNDS", 1), \
                    patch.object(perf_smoke, "run_once", side_effect=measure), \
                    patch.object(perf_smoke, "probe_layout", side_effect=probe), \
                    patch.object(perf_smoke, "add_summary"), patch("builtins.print") as errors, \
                    self.assertRaises(SystemExit):
                perf_smoke.main()
            errors.assert_any_call("::error::container-units-layout candidate did not use the expected two-pass layout path")

            def control_probe(binary, *arguments):
                return 1200, {
                    "layouts": 1000, "cq_passes": {"1": 1000},
                    "cq_termination": {"no-queries": 1000},
                    "retained_fallback": int(binary == candidate),
                }

            arguments[-1] = "div-text-layout"
            with patch.object(sys, "argv", arguments), patch.object(perf_smoke, "ROUNDS", 1), \
                    patch.object(perf_smoke, "run_once", return_value=(100.0, 8192, 100.0, 1200, "")), \
                    patch.object(perf_smoke, "probe_layout", side_effect=control_probe), \
                    patch.object(perf_smoke, "add_summary"), patch("builtins.print") as errors, \
                    self.assertRaises(SystemExit):
                perf_smoke.main()
            errors.assert_any_call("::error::div-text-layout candidate increased retained layout fallbacks")

    def test_hit_tests_skip_layout_telemetry_and_report_before_failing(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base, candidate, output = root / "base", root / "candidate", root / "results.json"
            base.touch()
            candidate.touch()

            def measure(binary, url, expression, expected, screenshot):
                return (130.0 if binary == candidate else 100.0), 8192, 50.0, expected, ""

            arguments = ["perf_smoke.py", "--base", str(base), "--candidate", str(candidate),
                         "--output", str(output), "--scenario", "hit-one-200", "--scenario", "hit-list-200"]
            with patch.object(sys, "argv", arguments), patch.object(perf_smoke, "ROUNDS", 2), \
                    patch.object(perf_smoke, "run_once", side_effect=measure), \
                    patch.object(perf_smoke, "probe_layout") as probe, \
                    patch.object(perf_smoke, "add_summary"), patch("builtins.print") as errors, \
                    self.assertRaises(SystemExit):
                perf_smoke.main()
            probe.assert_not_called()
            report = json.loads(output.read_text())["scenarios"]
            single, listed = report["hit-one-200"], report["hit-list-200"]
            self.assertEqual(single["pair_order"], [["base", "candidate"], ["candidate", "base"]])
            self.assertEqual(single["evaluations"], {"base": [200, 200], "candidate": [200, 200]})
            self.assertEqual(single["samples"]["candidate"]["cpu_ms"], [50.0, 50.0])
            self.assertAlmostEqual(single["latency_ratio"], 1.3)
            self.assertEqual(listed["pair_order"], [["candidate"], ["candidate"]])
            self.assertEqual(listed["evaluations"], {"candidate": [200, 200]})
            self.assertIsNone(listed["latency_ratio"])
            self.assertEqual(listed["telemetry"], {})
            failures = [call.args[0] for call in errors.call_args_list if str(call.args[0]).startswith("::error::")]
            self.assertEqual(failures, ["::error::hit-one-200 latency is 1.30x the base"])


if __name__ == "__main__":
    unittest.main()
