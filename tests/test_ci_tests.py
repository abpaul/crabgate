import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest


RUNNER = Path(__file__).resolve().parents[1] / "scripts" / "ci-test.py"


class IsolatedRustTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crabgate-ci-runner-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.config = self.root / "config.json"
        self.calls = self.root / "calls.jsonl"
        self.manifest = self.root / "Cargo.toml"
        self.manifest.write_text('[package]\nname="fixture"\nversion="0.1.0"\n')
        self.program("cargo", '''
import json,os,sys
from pathlib import Path
config=json.loads(Path(os.environ['RUNNER_CONFIG']).read_text())
def record(kind):
 fd=os.open(os.environ['RUNNER_CALLS'],os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)
 os.write(fd,(json.dumps(dict(kind=kind,args=sys.argv[1:],manifest_env=os.environ.get('CARGO_MANIFEST_DIR')))+'\\n').encode());os.close(fd)
if '--no-run' in sys.argv:
 record('build')
 if config.get('build_exit'): raise SystemExit(7)
 if config.get('bad_json'): print('not cargo JSON');raise SystemExit(0)
 print(json.dumps(dict(reason='compiler-artifact',profile=dict(test=True),target=dict(name='fixture',kind=config.get('target_kind',['test'])),executable=os.environ['RUNNER_BINARY'],manifest_path=os.environ['RUNNER_MANIFEST'],package_id='fixture 0.1.0')))
 if not config.get('missing_finished'): print(json.dumps(dict(reason='build-finished',success=True)))
elif '--doc' in sys.argv:
 record('doc')
 if not config.get('missing_doc_summary'):
  print('test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s')
 raise SystemExit(config.get('doc_exit',0))
else: raise SystemExit('unexpected Cargo invocation')
''')
        self.executable = self.program("fixture-tests", '''
import json,os,sqlite3,subprocess,sys,time
from pathlib import Path
config=json.loads(Path(os.environ['RUNNER_CONFIG']).read_text());names=config.get('names',['first','second','ignored']);ignored=config.get('ignored',['ignored']);benchmarks=config.get('benchmarks',[])
if '--list' in sys.argv:
 if config.get('bad_inventory'): print('unparseable inventory');raise SystemExit(0)
 for name in names:
  if '--ignored' not in sys.argv or name in ignored: print(name+': test')
 if '--ignored' not in sys.argv:
  for name in benchmarks: print(name+': benchmark')
 if config.get('delete_binary') and '--ignored' in sys.argv: Path(sys.argv[0]).unlink()
 raise SystemExit(0)
name=sys.argv[1]
assert sys.argv[2:]==['--exact','--test-threads=1','--color=never']
root=Path(os.environ['CRABGATE_TEST_ROOT'])
env={key:os.environ[key] for key in ['DATABASE_URL','CRABGATE_ORGANIZER_DATABASE_DIR','CRABGATE_TEST_ROOT','CRABGATE_TEST_EVIDENCE_DIR','CG032_TEST_ROOT','CG032_EVIDENCE_DIR','TMPDIR','TMP','TEMP','CARGO_MANIFEST_DIR']}
database=sqlite3.connect(env['DATABASE_URL'].removeprefix('sqlite://').split('?')[0]);database.execute('CREATE TABLE fixture(name TEXT)');database.execute('INSERT INTO fixture VALUES(?)',[name]);database.commit();assert database.execute('SELECT name FROM fixture').fetchall()==[(name,)];database.close()
(root/'environment.json').write_text(json.dumps(env))
fd=os.open(os.environ['RUNNER_CALLS'],os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600);os.write(fd,(json.dumps(dict(kind='test',name=name,root=str(root)))+'\\n').encode());os.close(fd)
mode=config.get('modes',{}).get(name,'pass')
if mode in ['timeout','child']:
 if mode=='child':
  child=subprocess.Popen([sys.executable,'-c',"import signal,time,pathlib;signal.signal(signal.SIGTERM,signal.SIG_IGN);p=pathlib.Path("+repr(str(root/'heartbeat'))+");\\nwhile True:p.write_text(str(time.time()));time.sleep(.05)"])
  (root/'child.pid').write_text(str(child.pid))
 time.sleep(120)
if mode=='missing': raise SystemExit(0)
status='FAILED' if mode=='failed' else 'ok'
print('test '+name+' ... '+status)
passed=2 if mode=='count' else 0 if mode in ['failed','ignored'] else 1
failed=1 if mode=='failed' else 0;skip=1 if mode=='ignored' else 0
print(f'test result: {status}. {passed} passed; {failed} failed; {skip} ignored; 0 measured; {len(names)-1} filtered out; finished in 0.01s')
raise SystemExit(9 if mode=='failed' else 0)
''')
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"], RUNNER_CONFIG=str(self.config), RUNNER_CALLS=str(self.calls), RUNNER_BINARY=str(self.executable), RUNNER_MANIFEST=str(self.manifest))
        self.config.write_text("{}")

    def program(self, name, source):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\n" + source)
        path.chmod(0o700)
        return path

    def command(self):
        return [sys.executable, str(RUNNER), "--evidence-root", str(self.root / "evidence"), "--workers", "2", "--test-timeout", "1"]

    def run_runner(self, config=None):
        self.config.write_text(json.dumps(config or {}))
        existing = set((self.root / "evidence").glob("rust-tests-*"))
        result = subprocess.run(self.command(), env=self.env, text=True, capture_output=True, timeout=20)
        roots = set((self.root / "evidence").glob("rust-tests-*")) - existing
        self.assertEqual(len(roots), 1)
        summary = json.loads((roots.pop() / "summary.json").read_text())
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []
        return result, summary, calls

    def test_build_once_exact_inventory_and_real_database_isolation(self):
        unusual = "quoted::test_`touch unwanted`"
        result, summary, calls = self.run_runner(dict(names=["first", unusual, "ignored"], ignored=["ignored"], benchmarks=["benchmark"]))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(summary["success"])
        self.assertEqual(summary["passed_tests"], 2)
        self.assertEqual([call["kind"] for call in calls].count("build"), 1)
        self.assertEqual([call["kind"] for call in calls].count("doc"), 1)
        tests = [call for call in calls if call["kind"] == "test"]
        self.assertCountEqual([call["name"] for call in tests], ["first", unusual])
        self.assertEqual(len({call["root"] for call in tests}), 2)
        self.assertEqual(sum(entry["ignored"] for entry in summary["inventory"]), 1)
        self.assertEqual([(entry["name"], entry["kind"]) for entry in summary["inventory"] if entry["kind"] == "benchmark"], [("benchmark", "benchmark")])
        self.assertEqual(len(summary["inventory_sha256"]), 64)
        self.assertLessEqual(summary["peak_children"], 2)
        for call in tests:
            root = Path(call["root"])
            with sqlite3.connect(root / "control.db") as database:
                self.assertEqual(database.execute("SELECT name FROM fixture").fetchall(), [(call["name"],)])
            env = json.loads((root / "environment.json").read_text())
            self.assertEqual(env["DATABASE_URL"], f"sqlite://{root}/control.db?mode=rwc")
            self.assertEqual(env["TMPDIR"], str(root / "tmp"))
            self.assertEqual(env["CG032_EVIDENCE_DIR"], str(root / "evidence"))
            self.assertEqual(env["CRABGATE_ORGANIZER_DATABASE_DIR"], str(root / "tenants"))
            self.assertEqual(env["CARGO_MANIFEST_DIR"], str(self.manifest.parent.resolve()))

    def test_build_and_doc_cargo_preserve_the_callers_manifest_environment(self):
        for value in (None, "inherited-caller-value"):
            with self.subTest(value=value):
                self.env.pop("CARGO_MANIFEST_DIR", None)
                if value is not None:
                    self.env["CARGO_MANIFEST_DIR"] = value
                result, summary, calls = self.run_runner()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                cargo = [call for call in calls if call["kind"] in ("build", "doc")][-2:]
                self.assertEqual([call["manifest_env"] for call in cargo], [value, value])
                self.assertEqual(summary["passed_tests"], 2)

    def test_wrong_pass_count_failure_missing_summary_and_ignored_execution_fail(self):
        config = dict(names=["count", "failed", "missing", "ignored_execution", "pass"], ignored=[], modes=dict(count="count", failed="failed", missing="missing", ignored_execution="ignored"))
        result, summary, calls = self.run_runner(config)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(summary["expected_tests"], 5)
        self.assertEqual(summary["passed_tests"], 1)
        self.assertEqual(len(summary["results"]), 5)
        self.assertCountEqual([call["name"] for call in calls if call["kind"] == "test"], config["names"])
        self.assertEqual([call["kind"] for call in calls].count("doc"), 1)
        self.assertEqual(len([row for row in summary["results"] if Path(row["log"]).exists()]), 5)

    def test_timeout_keeps_other_tests_and_doc_tests_running(self):
        result, summary, calls = self.run_runner(dict(names=["slow", "pass"], ignored=[], modes=dict(slow="timeout")))
        self.assertEqual(result.returncode, 1)
        self.assertEqual(summary["passed_tests"], 1)
        self.assertTrue(next(row for row in summary["results"] if row["name"] == "slow")["timed_out"])
        self.assertEqual([call["kind"] for call in calls].count("doc"), 1)

    def test_cli_rejects_unbounded_workers_and_invalid_timeouts_before_build(self):
        for extra in (["--workers", "0"], ["--workers", "9"], ["--test-timeout", "0"], ["--test-timeout", "nan"], ["--build-timeout", "inf"], ["--doc-timeout", "-1"]):
            with self.subTest(extra=extra):
                result = subprocess.run(self.command() + extra, env=self.env, text=True, capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertIn("error:", result.stderr)
                self.assertFalse(self.calls.exists())

    def test_invalid_build_and_inventory_do_not_report_success(self):
        for config in [dict(build_exit=True), dict(bad_json=True), dict(missing_finished=True), dict(bad_inventory=True), dict(names=[], ignored=[]), dict(names=[], ignored=[], target_kind=["lib"])]:
            with self.subTest(config=config):
                result, summary, _ = self.run_runner(config)
                self.assertEqual(result.returncode, 1)
                self.assertFalse(summary["success"])
                self.assertIn("error", summary)

    def test_missing_executables_fail_every_selected_test_and_keep_doc_tests(self):
        result, summary, calls = self.run_runner(dict(delete_binary=True))
        self.assertEqual(result.returncode, 1)
        self.assertEqual(summary["expected_tests"], 2)
        self.assertEqual(summary["passed_tests"], 0)
        self.assertEqual(len(summary["results"]), 2)
        self.assertTrue(all(row["exit_code"] is None for row in summary["results"]))
        self.assertTrue(all(Path(row["log"]).read_text() for row in summary["results"]))
        self.assertEqual([call["kind"] for call in calls].count("doc"), 1)

    def test_doc_failure_and_missing_doc_summary_fail(self):
        for config in [dict(doc_exit=7), dict(missing_doc_summary=True)]:
            with self.subTest(config=config):
                result, summary, _ = self.run_runner(config)
                self.assertEqual(result.returncode, 1)
                self.assertFalse(summary["success"])
                self.assertEqual(summary["passed_tests"], 2)

    def test_cancellation_stops_owned_descendants_and_preserves_other_process(self):
        self.config.write_text(json.dumps(dict(names=["child"], ignored=[], modes=dict(child="child"))))
        sentinel = subprocess.Popen([sys.executable, "-c", "import time;time.sleep(120)"])
        runner = subprocess.Popen(self.command(), env=self.env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        try:
            deadline = time.monotonic() + 10
            heartbeat = None
            while time.monotonic() < deadline:
                files = list((self.root / "evidence").glob("rust-tests-*/test-*/heartbeat"))
                if files:
                    heartbeat = files[0]
                    break
                time.sleep(0.02)
            self.assertIsNotNone(heartbeat)
            runner.send_signal(signal.SIGTERM)
            output, _ = runner.communicate(timeout=10)
            self.assertEqual(runner.returncode, 1, output)
            prior = heartbeat.read_text()
            time.sleep(0.15)
            self.assertEqual(heartbeat.read_text(), prior)
            self.assertIsNone(sentinel.poll())
            root = heartbeat.parents[1]
            summary = json.loads((root / "summary.json").read_text())
            self.assertTrue(summary["cancelled"])
            self.assertFalse(summary["success"])
        finally:
            for process in (runner, sentinel):
                if process.poll() is None:
                    process.kill()
                process.wait()


if __name__ == "__main__":
    unittest.main()
