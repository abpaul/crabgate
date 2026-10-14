import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class TenantOperations(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cg018-script-")
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        (self.root / "scripts").mkdir()
        (self.root / "bin").mkdir()
        for name in ("deploy.sh", "tenant-smoke.sh"):
            shutil.copy2(Path(__file__).resolve().parents[1] / "scripts" / name,
                         self.root / "scripts" / name)
        self.cookies = self.root / "operator.cookies"
        self.cookies.write_text("SECRET_COOKIE_MUST_NOT_APPEAR")
        self.cookies.chmod(0o600)
        self.log = self.root / "calls"
        self.store = self.root / "image-store"
        self.store.write_text("")
        self.env = {"PATH": str(self.root / "bin") + ":/usr/bin:/bin",
                    "FAKE_LOG": str(self.log), "FAKE_STORE": str(self.store), "FAKE_HEALTH": "healthy",
                    "FAKE_READINESS": "200", "FAKE_ROUTE": "200",
                    "FAKE_BODY": json.dumps({"organizer_id": "00000000-0000-0000-0000-000000000018", "ready": True, "latest_migration": "m20261007_000022_series_exceptions"})}
        self.fake("docker", '''printf '%s\\n' "$*" >> "$FAKE_LOG"
current=$(sed -n 's/^CRABGATE_IMAGE=//p' .deploy.env 2>/dev/null || true)
forget() {
  grep -Fxv -- "$1" "$FAKE_STORE" > "$FAKE_STORE.next" || true
  mv "$FAKE_STORE.next" "$FAKE_STORE"
}
case "$1" in
inspect)
  if [ -n "${FAKE_UNHEALTHY_IMAGE:-}" ] && grep -Fxq "CRABGATE_IMAGE=$FAKE_UNHEALTHY_IMAGE" .deploy.env; then
    echo unhealthy
  else
    echo "$FAKE_HEALTH"
  fi;;
image) case "$2" in
  ls) grep -F -- "${5:-}:" "$FAKE_STORE" || true;;
  rm) forget "$3";;
  prune) : > "$FAKE_STORE";;
  esac;;
rmi) forget "$2";;
system) : > "$FAKE_STORE";;
compose) case "$*" in
  *"ps -q app"*) echo fixture-app;;
  *"pull app"*)
    [ "${FAKE_PULL_EXIT:-0}" = 0 ] || exit "$FAKE_PULL_EXIT"
    grep -Fxq -- "$current" "$FAKE_STORE" || echo "$current" >> "$FAKE_STORE";;
  *"up -d"*) case " ${FAKE_UP_FAIL:-} " in *" $current "*) exit 1;; esac;;
  esac;;
esac
''')
        self.fake("curl", '''printf '%s\\n' "$*" >> "$FAKE_LOG"
[ "${FAKE_CURL_EXIT:-0}" = 0 ] || exit "$FAKE_CURL_EXIT"
for arg in "$@"; do url=$arg; done
case "$url" in */readiness) printf '%s\\n%s' "$FAKE_BODY" "$FAKE_READINESS";; *) printf '%s' "$FAKE_ROUTE";; esac
''')
        self.fake("sleep", "exit 0\n")
        self.args = ["https://shop.example.com", "00000000-0000-0000-0000-000000000018", str(self.cookies)]

    def fake(self, name, body):
        path = self.root / "bin" / name
        path.write_text("#!/bin/sh\n" + body)
        path.chmod(0o755)

    def run_script(self, name, args, **env):
        result = subprocess.run(["sh", str(self.root / "scripts" / name), *args],
                                env=dict(self.env, **env), text=True, capture_output=True)
        self.assertNotIn("SECRET_COOKIE_MUST_NOT_APPEAR", result.stdout + result.stderr)
        return result

    def test_smoke_success_calls_target_readiness_and_root(self):
        result = self.run_script("tenant-smoke.sh", self.args)
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.log.read_text()
        self.assertTrue(all(call.startswith("-q ") for call in calls.splitlines()))
        self.assertIn("https://shop.example.com/operator/organizers/" + self.args[1] + "/readiness", calls)
        self.assertIn("-- https://shop.example.com/", calls)
        self.assertNotIn("SECRET_COOKIE_MUST_NOT_APPEAR", calls)

    def test_smoke_failures_and_redirects_do_not_pass(self):
        for status in ("403", "404", "503", "302"):
            with self.subTest(status=status):
                result = self.run_script("tenant-smoke.sh", self.args, FAKE_READINESS=status)
                self.assertEqual(result.returncode, 1)
                self.assertIn("HTTP " + status, result.stderr)
        result = self.run_script("tenant-smoke.sh", self.args, FAKE_ROUTE="503")
        self.assertEqual(result.returncode, 1)
        result = self.run_script("tenant-smoke.sh", self.args, FAKE_CURL_EXIT="7")
        self.assertEqual(result.returncode, 1)

    def test_smoke_invalid_arguments(self):
        for args in ([], [*self.args[:1], "invalid", self.args[2]],
                     ["http://public.example.com", *self.args[1:]],
                     [self.args[0], self.args[1] + "\n", self.args[2]],
                     [*self.args[:2], str(self.root / "missing")]):
            self.assertEqual(self.run_script("tenant-smoke.sh", args).returncode, 2)

    def test_smoke_rejects_origin_components_before_curl(self):
        for origin in ("https://shop.example.com/", "https://shop.example.com/?",
                       "https://shop.example.com/path", "https://shop.example.com?x=1",
                       "https://shop.example.com#fragment", "https://user:password@shop.example.com",
                       "https://shop.example.com\nhttps://other.example.com",
                       "http://localhost:80/?", "https://", "https://shop.example.com:port"):
            with self.subTest(origin=origin):
                result = self.run_script("tenant-smoke.sh", [origin, *self.args[1:]])
                self.assertEqual(result.returncode, 2)
                self.assertFalse(self.log.exists())

    def test_smoke_rejects_unrelated_or_unready_200_body(self):
        for body in ("<html>marketing</html>", "{}",
                     json.dumps({"organizer_id": self.args[1], "ready": False, "latest_migration": "m1"}),
                     json.dumps({"organizer_id": "00000000-0000-0000-0000-000000000019", "ready": True, "latest_migration": "m1"}),
                     '{"ready":true,"ready":true,"organizer_id":"' + self.args[1] + '"}',
                     json.dumps({"organizer_id": self.args[1], "ready": "true", "latest_migration": "m1"})):
            with self.subTest(body=body):
                self.assertEqual(self.run_script("tenant-smoke.sh", self.args, FAKE_BODY=body).returncode, 1)

    def test_real_http_body_cookie_and_config_suppression(self):
        calls = []
        target = self.args[1]
        readiness = {"organizer_id": target, "ready": True, "latest_migration": "m1"}

        class Handler(BaseHTTPRequestHandler):
            body = json.dumps(readiness, indent=2)

            def do_GET(self):
                calls.append((self.path, self.headers.get("Cookie"), self.headers.get("X-Inherited")))
                body = self.body if self.path.endswith("/readiness") else "<html>shop</html>"
                self.send_response(200)
                self.send_header("Content-Length", str(len(body.encode())))
                self.end_headers()
                self.wfile.write(body.encode())

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever)
        thread.start()
        try:
            (self.root / ".curlrc").write_text('verbose\nheader = "X-Inherited: unsafe"\n')
            self.cookies.write_text("# Netscape HTTP Cookie File\n127.0.0.1\tFALSE\t/\tFALSE\t0\tcg_operator_session\tSECRET_COOKIE_MUST_NOT_APPEAR\n")
            args = [f"http://127.0.0.1:{server.server_port}", target, str(self.cookies)]
            for body, expected in ((json.dumps(readiness, indent=2), 0),
                                   ("<html>marketing</html>", 1),
                                   (json.dumps(dict(readiness, ready=False)), 1),
                                   (json.dumps(dict(readiness, organizer_id="00000000-0000-0000-0000-000000000019")), 1)):
                Handler.body = body
                calls.clear()
                result = self.run_script("tenant-smoke.sh", args, PATH="/usr/bin:/bin", CURL_HOME=str(self.root))
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(calls[0], (f"/operator/organizers/{target}/readiness", "cg_operator_session=SECRET_COOKIE_MUST_NOT_APPEAR", None))
                self.assertEqual(len(calls), 2 if expected == 0 else 1)
                if expected == 0:
                    self.assertEqual(calls[1], ("/", None, None))
            calls.clear()
            self.assertEqual(self.run_script("tenant-smoke.sh", [args[0] + "/?", *args[1:]], PATH="/usr/bin:/bin", CURL_HOME=str(self.root)).returncode, 2)
            self.assertEqual(calls, [])
        finally:
            server.shutdown()
            thread.join()
            server.server_close()

    COMPOSE = "compose --env-file .env --env-file .deploy.env -f compose.yaml -f compose.hosted.yaml "
    HEALTH_PASSED = ("Image health: passed. fixture:new is running and its container health check passed. "
                     "This does not verify Organizer migrations.")

    def record(self, image=None):
        path = self.root / ".deploy.env"
        if image:
            path.write_text("CRABGATE_IMAGE=" + image + "\n")
        return path.read_text() if path.exists() else None

    def docker_calls(self):
        return [call for call in self.log.read_text().splitlines() if not call.startswith("-q ")]

    def curl_calls(self):
        return [call for call in self.log.read_text().splitlines() if call.startswith("-q ")]

    def images(self):
        return self.store.read_text().splitlines()

    def test_missing_tenant_smoke_arguments_change_nothing(self):
        self.record("fixture:old")
        for args in (["fixture:new"], ["fixture:new", "partial"], ["fixture:new", *self.args[:2]],
                     ["fixture:new", "", *self.args[1:]], ["fixture:new", self.args[0], "", self.args[2]],
                     ["", *self.args], ["fixture:new", *self.args[:2], ""],
                     ["fixture:new\nSHOP_BASE_DOMAIN=evil.example", *self.args], ["fixture:new\r", *self.args],
                     ["fixture:\tnew", *self.args], ["fixture:new\x7f", *self.args], ["fixture new", *self.args],
                     ["fixture:new", *self.args[:2], str(self.root / "missing.cookies")]):
            with self.subTest(args=args):
                result = self.run_script("deploy.sh", args)
                self.assertEqual(result.returncode, 2)
                self.assertIn("Nothing was changed.", result.stderr)
                self.assertEqual(result.stdout, "")
                self.assertFalse(self.log.exists())
                self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:old\n")

    def test_missing_smoke_script_changes_nothing(self):
        (self.root / "scripts/tenant-smoke.sh").unlink()
        result = self.run_script("deploy.sh", ["fixture:new", *self.args])
        self.assertEqual(result.returncode, 2)
        self.assertFalse(self.log.exists())
        self.assertIsNone(self.record())

    def test_health_and_tenant_acceptance_are_reported_separately(self):
        self.record("fixture:old")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.splitlines(), [
            self.HEALTH_PASSED,
            "Organizer 00000000-0000-0000-0000-000000000018 readiness and tenant route passed",
            "Tenant acceptance: passed for Organizer 00000000-0000-0000-0000-000000000018 at https://shop.example.com. Other Organizers were not checked.",
            "Deployed fixture:new. Previously recorded image: fixture:old."])
        self.assertEqual(self.docker_calls()[:4], [
            self.COMPOSE + "pull app",
            self.COMPOSE + "up -d --no-build",
            self.COMPOSE + "ps -q app",
            "inspect --format {{.State.Health.Status}} fixture-app"])
        self.assertEqual(len(self.curl_calls()), 2)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:new\n")

    def test_registry_tag_and_digest_references_are_deployed(self):
        for image in ("ghcr.io/alplus-tech/crabgate:e7d4c47eba3d6cd4c598633267c6208da60fa0a8",
                      "ghcr.io/alplus-tech/crabgate@sha256:" + "0123456789abcdef" * 4,
                      "localhost:5000/crab_gate:v1.2-rc_3"):
            with self.subTest(image=image):
                result = self.run_script("deploy.sh", [image, *self.args])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.record(), "CRABGATE_IMAGE=" + image + "\n")
                self.assertEqual(self.images()[-1], image)

    def test_successive_deploys_and_reruns_remove_no_image(self):
        repository = "ghcr.io/alplus-tech/crabgate:"
        self.store.write_text(repository + "known-good\nother.example/tool:1\n")
        runs = [self.run_script("deploy.sh", [repository + tag, *self.args], FAKE_READINESS=readiness)
                for tag, readiness in (("a", "200"), ("b", "200"), ("b", "200"), ("c", "401"), ("c", "200"))]
        self.assertEqual([run.returncode for run in runs], [0, 0, 0, 1, 0])
        self.assertEqual(self.images(), [repository + "known-good", "other.example/tool:1",
                                         repository + "a", repository + "b", repository + "c"])
        self.assertEqual({call.split()[0] for call in self.docker_calls()}, {"compose", "inspect"})
        self.assertEqual([run.stdout.splitlines()[-1] for run in runs if run.returncode == 0], [
            "Deployed " + repository + "a. No previous image is recorded.",
            "Deployed " + repository + "b. Previously recorded image: " + repository + "a.",
            "Deployed " + repository + "b. " + repository + "b was already the recorded image.",
            "Deployed " + repository + "c. " + repository + "c was already the recorded image."])
        self.assertIn(" Previously recorded image: " + repository + "b. ", runs[3].stderr)
        self.assertNotIn("Rollback image kept", "".join(run.stdout + run.stderr for run in runs))

    def test_failed_tenant_acceptance_keeps_image_and_every_local_image(self):
        self.record("fixture:old")
        self.store.write_text("fixture:older\nfixture:old\n")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_READINESS="503")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout.splitlines(), [self.HEALTH_PASSED])
        self.assertIn("Tenant acceptance: FAILED for Organizer 00000000-0000-0000-0000-000000000018 at https://shop.example.com.", result.stderr)
        self.assertIn("does not reverse database migrations", result.stderr)
        self.assertIn("Previously recorded image: fixture:old.", result.stderr)
        self.assertNotIn("Deployed", result.stdout + result.stderr)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:new\n")
        calls = self.docker_calls()
        self.assertEqual(calls.count(self.COMPOSE + "up -d --no-build"), 1)
        self.assertEqual(self.images(), ["fixture:older", "fixture:old", "fixture:new"])

    def test_unhealthy_image_rolls_back_without_tenant_smoke(self):
        self.record("fixture:old")
        self.store.write_text("fixture:older\nfixture:old\n")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_UNHEALTHY_IMAGE="fixture:new")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("Image health: FAILED. fixture:new did not become healthy. Tenant acceptance was not run.", result.stderr)
        self.assertIn("Starting the previous image again: fixture:old. Database migrations that fixture:new applied are not reversed.", result.stderr)
        self.assertIn("Rollback image health: passed for fixture:old.", result.stderr)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:old\n")
        calls = self.docker_calls()
        self.assertEqual(calls.count(self.COMPOSE + "up -d --no-build"), 2)
        self.assertEqual(self.images(), ["fixture:older", "fixture:old", "fixture:new"])
        self.assertEqual(self.curl_calls(), [])

    def test_failed_compose_up_rolls_back_without_tenant_smoke(self):
        self.record("fixture:old")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_UP_FAIL="fixture:new")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("Image health: FAILED. fixture:new did not become healthy. Tenant acceptance was not run.", result.stderr)
        self.assertIn("Rollback image health: passed for fixture:old.", result.stderr)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:old\n")
        self.assertEqual(self.docker_calls().count(self.COMPOSE + "up -d --no-build"), 2)
        self.assertEqual(self.curl_calls(), [])

    def test_failed_compose_up_of_new_and_previous_image_is_reported(self):
        self.record("fixture:old")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_UP_FAIL="fixture:new fixture:old")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Rollback image health: FAILED for fixture:old. The service needs operator recovery.", result.stderr)
        self.assertNotIn("Rollback image health: passed", result.stderr)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:old\n")
        self.assertEqual(self.docker_calls().count(self.COMPOSE + "up -d --no-build"), 2)
        self.assertEqual(self.curl_calls(), [])

    def test_unhealthy_rollback_image_is_reported(self):
        self.record("fixture:old")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_HEALTH="unhealthy")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Rollback image health: FAILED for fixture:old. The service needs operator recovery.", result.stderr)
        self.assertNotIn("Rollback image health: passed", result.stderr)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:old\n")
        self.assertEqual(self.curl_calls(), [])

    def test_unhealthy_first_deploy_removes_image_override(self):
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_HEALTH="unhealthy")
        self.assertEqual(result.returncode, 1)
        self.assertIn("No previous image is recorded, so no rollback is possible.", result.stderr)
        self.assertIsNone(self.record())
        self.assertEqual(self.docker_calls().count(self.COMPOSE + "up -d --no-build"), 1)

    def test_unhealthy_redeploy_of_recorded_image_keeps_its_record(self):
        self.record("fixture:new")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_HEALTH="unhealthy")
        self.assertEqual(result.returncode, 1)
        self.assertIn("fixture:new was already the recorded image, so no rollback is possible.", result.stderr)
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:new\n")

    def test_failed_pull_keeps_recorded_image_and_running_stack(self):
        self.record("fixture:old")
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_PULL_EXIT="1")
        self.assertEqual(result.returncode, 1)
        self.assertIn("Could not pull fixture:new. The running stack was not changed.", result.stderr)
        self.assertEqual(self.docker_calls(), [self.COMPOSE + "pull app"])
        self.assertEqual(self.record(), "CRABGATE_IMAGE=fixture:old\n")

    def test_failed_first_pull_leaves_no_image_record(self):
        result = self.run_script("deploy.sh", ["fixture:new", *self.args], FAKE_PULL_EXIT="1")
        self.assertEqual(result.returncode, 1)
        self.assertIsNone(self.record())


if __name__ == "__main__":
    unittest.main()
