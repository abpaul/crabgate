#!/usr/bin/env python3
import argparse
import concurrent.futures
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time


RESULT = re.compile(
    r"^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"(\d+) measured; (\d+) filtered out; finished in [\d.]+s$", re.MULTILINE
)
PROFILE = 'profile.dev.package."*".debug=false'


def isolated_environment(root, cwd, set_manifest_dir):
    root.mkdir(parents=True, exist_ok=False)
    for child in ("tenants", "evidence", "tmp"):
        (root / child).mkdir()
    env = dict(
        os.environ,
        CRABGATE_EDITION="self-hosted",
        DATABASE_URL=f"sqlite://{root}/control.db?mode=rwc",
        CRABGATE_ORGANIZER_DATABASE_DIR=str(root / "tenants"),
        CRABGATE_TEST_ROOT=str(root),
        CRABGATE_TEST_EVIDENCE_DIR=str(root / "evidence"),
        CG032_TEST_ROOT=str(root / "evidence"),
        CG032_EVIDENCE_DIR=str(root / "evidence"),
        TMPDIR=str(root / "tmp"),
        TMP=str(root / "tmp"),
        TEMP=str(root / "tmp"),
    )
    if set_manifest_dir:
        env["CARGO_MANIFEST_DIR"] = str(cwd)
    return env


class Processes:
    def __init__(self):
        self.cancelled = threading.Event()
        self.lock = threading.Lock()
        self.active = {}
        self.peak = 0

    def cancel(self, *_):
        self.cancelled.set()

    @staticmethod
    def stop(process):
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()

    def run(self, command, root, cwd, timeout, separate_stderr=False, set_manifest_dir=True):
        start = time.monotonic()
        result = dict(command=command, log=str(root / "stdout.log"), exit_code=None)
        env = isolated_environment(root, cwd, set_manifest_dir)
        with (root / "stdout.log").open("w") as output, (root / "stderr.log").open("w") as errors:
            with self.lock:
                if self.cancelled.is_set():
                    return dict(result, cancelled=True, seconds=0)
                try:
                    process = subprocess.Popen(
                        command, cwd=cwd, env=env, stdout=output,
                        stderr=errors if separate_stderr else subprocess.STDOUT,
                        start_new_session=True,
                    )
                except OSError as error:
                    output.write(str(error) + "\n")
                    return dict(result, error=str(error), seconds=time.monotonic() - start)
                self.active[process.pid] = process
                self.peak = max(self.peak, len(self.active))
            try:
                while process.poll() is None:
                    if self.cancelled.is_set() or time.monotonic() - start >= timeout:
                        result["cancelled" if self.cancelled.is_set() else "timed_out"] = True
                        self.stop(process)
                        break
                    try:
                        process.wait(timeout=0.1)
                    except subprocess.TimeoutExpired:
                        pass
                if self.cancelled.is_set() and not result.get("cancelled"):
                    result["cancelled"] = True
                    self.stop(process)
                result["exit_code"] = process.returncode
            finally:
                if process.poll() is None:
                    self.stop(process)
                with self.lock:
                    self.active.pop(process.pid)
            result["seconds"] = time.monotonic() - start
            return result


def artifacts_from_json(path):
    artifacts = {}
    finished = []
    with path.open() as source:
        for line in source:
            if not line.strip():
                continue
            message = json.loads(line)
            if not isinstance(message, dict):
                raise ValueError("Cargo emitted a non-object JSON message")
            if message.get("reason") == "build-finished":
                finished.append(message.get("success"))
            if message.get("reason") != "compiler-artifact" or not message.get("profile", {}).get("test"):
                continue
            executable = message.get("executable")
            if executable is None:
                continue
            path = Path(executable).resolve(strict=True)
            if not path.is_file() or not os.access(path, os.X_OK):
                raise ValueError(f"Missing executable: {path}")
            target = message["target"]
            if not isinstance(target["kind"], list) or not isinstance(target["name"], str):
                raise ValueError("Invalid Cargo test target")
            artifact = dict(
                executable=str(path), name=target["name"], kind=target["kind"],
                manifest=str(Path(message["manifest_path"]).resolve(strict=True)),
                package_id=message["package_id"],
                sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
            )
            if executable in artifacts and artifacts[executable] != artifact:
                raise ValueError("Conflicting Cargo executable artifacts")
            artifacts[executable] = artifact
    if finished != [True] or not artifacts:
        raise ValueError("Cargo did not report one successful complete test build")
    return sorted(artifacts.values(), key=lambda item: item["executable"])


def parse_listing(text):
    entries = []
    for line in text.splitlines():
        match = re.fullmatch(r"(.+): (test|benchmark)", line)
        if not match or "\x00" in match[1]:
            raise ValueError(f"Unparseable libtest inventory: {line[:160]!r}")
        entry = (match[1], match[2])
        if entry in entries:
            raise ValueError("Duplicate libtest inventory entry")
        entries.append(entry)
    return entries


def succeeded(result):
    return result["exit_code"] == 0 and not result.get("cancelled") and not result.get("timed_out")


def discover(artifacts, processes, root, timeout):
    inventory = []
    for i, artifact in enumerate(artifacts):
        lists = []
        for label, extra in (("all", []), ("ignored", ["--ignored"])):
            result = processes.run(
                [artifact["executable"], "--list", "--format", "terse", *extra],
                root / f"inventory-{i}-{label}", Path(artifact["manifest"]).parent, timeout,
            )
            if not succeeded(result):
                raise ValueError(f"Inventory process failed: {result['log']}")
            lists.append(parse_listing(Path(result["log"]).read_text(errors="replace")))
        all_entries, ignored = lists
        if not set(ignored).issubset(all_entries):
            raise ValueError("Ignored inventory contains undiscovered tests")
        if any(kind in artifact["kind"] for kind in ("test", "lib")) and not all_entries:
            raise ValueError(f"Test target has no inventory: {artifact['name']}")
        for name, kind in all_entries:
            inventory.append(dict(artifact=artifact, name=name, kind=kind, ignored=(name, kind) in ignored))
    if not any(entry["kind"] == "test" and not entry["ignored"] for entry in inventory):
        raise ValueError("No nonignored Rust tests discovered")
    return inventory


def execute_test(entry, index, processes, root, timeout):
    artifact = entry["artifact"]
    result = processes.run(
        [artifact["executable"], entry["name"], "--exact", "--test-threads=1", "--color=never"],
        root / f"test-{index:05}", Path(artifact["manifest"]).parent, timeout,
    )
    result.update(name=entry["name"], target=artifact["name"])
    text = Path(result["log"]).read_text(errors="replace")
    summaries = RESULT.findall(text)
    named_pass = re.search(r"^test " + re.escape(entry["name"]) + r" \.\.\. ok$", text, re.MULTILINE)
    result["verified"] = bool(succeeded(result) and len(summaries) == 1 and summaries[0][:5] == ("ok", "1", "0", "0", "0") and named_pass)
    if not result["verified"]:
        result["error"] = "Expected this exact named test to pass once without ignored or measured tests"
    return result


def execute_tests(inventory, workers, processes, root, timeout, summary, save):
    selected = [entry for entry in inventory if entry["kind"] == "test" and not entry["ignored"]]
    summary["expected_tests"] = len(selected)
    summary["results"] = []
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
        futures = [pool.submit(execute_test, entry, i, processes, root, timeout) for i, entry in enumerate(selected)]
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            summary["results"].append(result)
            save()
            if not result["verified"]:
                print(f"FAIL {result['target']}::{result['name']} — {result['log']}", flush=True)
    summary["test_seconds"] = time.monotonic() - start
    summary["passed_tests"] = sum(result["verified"] for result in summary["results"])
    return summary["passed_tests"] == len(selected)


def positive_seconds(value):
    seconds = float(value)
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("Timeout must be positive and finite")
    return seconds


def main(argv=None):
    start = time.monotonic()
    parser = argparse.ArgumentParser(description="Build once and run every nonignored Rust test in isolated bounded processes.")
    parser.add_argument("--workers", type=int, choices=range(1, 9), default=4)
    parser.add_argument("--test-timeout", type=positive_seconds, default=180)
    parser.add_argument("--build-timeout", type=positive_seconds, default=900)
    parser.add_argument("--doc-timeout", type=positive_seconds, default=300)
    parser.add_argument("--toolchain")
    parser.add_argument("--evidence-root", type=Path, default=Path(os.environ.get("CRABGATE_TEST_ROOT", tempfile.gettempdir())))
    parser.add_argument("--build-manifest", type=Path, help="Reuse saved Cargo JSON only for local execution measurements")
    args = parser.parse_args(argv)
    args.evidence_root.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="rust-tests-", dir=args.evidence_root)).resolve()
    cwd = Path(__file__).resolve().parents[1]
    cargo = ["cargo", *([f"+{args.toolchain}"] if args.toolchain else []), "test", "--config", PROFILE, "--locked"]
    processes = Processes()
    handlers = {sig: signal.signal(sig, processes.cancel) for sig in (signal.SIGINT, signal.SIGTERM)}
    summary = dict(workers=args.workers, evidence_root=str(root), disk_before=shutil.disk_usage(root)._asdict())

    def save():
        summary["peak_children"] = processes.peak
        (root / "summary.json").write_text(json.dumps(summary, indent=2))

    try:
        if args.build_manifest:
            manifest = args.build_manifest.resolve(strict=True)
            summary["build"] = dict(reused_manifest=str(manifest), seconds=0)
        else:
            summary["build"] = processes.run(cargo + ["--no-run", "--message-format=json"], root / "build", cwd, args.build_timeout, separate_stderr=True, set_manifest_dir=False)
            manifest = Path(summary["build"]["log"])
            if not succeeded(summary["build"]):
                raise ValueError(f"Cargo build failed: {root / 'build' / 'stderr.log'}")
        inventory_start = time.monotonic()
        artifacts = artifacts_from_json(manifest)
        inventory = discover(artifacts, processes, root, args.test_timeout)
        summary["inventory_seconds"] = time.monotonic() - inventory_start
        summary["inventory"] = inventory
        summary["inventory_sha256"] = hashlib.sha256(json.dumps(inventory, sort_keys=True).encode()).hexdigest()
        save()
        tests_ok = execute_tests(inventory, args.workers, processes, root, args.test_timeout, summary, save)
        summary["doc_tests"] = processes.run(cargo + ["--doc", "--no-fail-fast", "--", "--color=never"], root / "doc", cwd, args.doc_timeout, set_manifest_dir=False)
        doc = summary["doc_tests"]
        doc["summaries"] = RESULT.findall(Path(doc["log"]).read_text(errors="replace"))
        doc_ok = succeeded(doc) and bool(doc["summaries"]) and all(row[0] == "ok" and row[2] == "0" for row in doc["summaries"])
        summary["success"] = bool(tests_ok and doc_ok and not processes.cancelled.is_set())
    except (ValueError, KeyError, TypeError, OSError) as error:
        summary["success"] = False
        summary["error"] = str(error)
        print(str(error), flush=True)
    finally:
        summary["cancelled"] = processes.cancelled.is_set()
        summary["total_seconds"] = time.monotonic() - start
        summary["disk_after"] = shutil.disk_usage(root)._asdict()
        summary["evidence_bytes"] = sum(path.stat().st_size for path in root.rglob("*") if path.is_file() and not path.is_symlink())
        save()
        for sig, handler in handlers.items():
            signal.signal(sig, handler)
    print(f"Rust tests: {summary.get('passed_tests', 0)}/{summary.get('expected_tests', 0)}; success={summary['success']}; evidence={root}", flush=True)
    return 0 if summary["success"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
