from contextlib import closing
from pathlib import Path
import fcntl
import hashlib
import json
import os
import pty
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import termios
import unittest


ORGANIZER = '00000000-0000-0000-0000-000000000032'
TIMESTAMP = '2026-10-09T00:00:00Z'
LITESTREAM_IMAGE = 'litestream/litestream:0.5.17'
DEPLOYED_IMAGE = 'registry.example/crabgate:deployed'
DEPLOYED_CONTEXT = ['--env-file', '.env', '--env-file', '.deploy.env',
                    '-f', 'compose.yaml', '-f', 'compose.hosted.yaml']
PREFLIGHT = [['compose', 'config', '--images', 'app', 'litestream']]


class OrganizerRestore(unittest.TestCase):
    def setUp(self):
        parent = os.environ.get('CG032_TEST_ROOT')
        if parent:
            self.root = Path(tempfile.mkdtemp(prefix='cg032-restore-', dir=parent))
        else:
            temporary = tempfile.TemporaryDirectory(prefix='cg032-restore-')
            self.addCleanup(temporary.cleanup)
            self.root = Path(temporary.name)
        self.root = self.root.resolve()
        (self.root / 'bin').mkdir()
        (self.root / 'data/organizers').mkdir(parents=True)
        self.target = self.root / 'data/organizers' / (ORGANIZER + '.db')
        self.snapshot = self.root / 'snapshot.db'
        self.create(self.snapshot, 'restored order', 'restored ticket')
        seed = self.root / 'seed.db'
        db = sqlite3.connect(seed)
        db.execute('PRAGMA journal_mode=WAL')
        db.execute('PRAGMA wal_autocheckpoint=0')
        db.executescript("CREATE TABLE orders (value TEXT); CREATE TABLE tickets (value TEXT);")
        db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
        db.execute("INSERT INTO orders VALUES ('original WAL order')")
        db.execute("INSERT INTO tickets VALUES ('original WAL ticket')")
        db.commit()
        for suffix in ('', '-wal', '-shm'):
            shutil.copyfile(str(seed) + suffix, str(self.target) + suffix)
        db.close()
        self.target.chmod(0o600)
        self.control = self.root / 'data/control.db'
        self.other = self.root / 'data/organizers/other.db'
        self.create(self.control, 'control session', 'control subscription')
        self.create(self.other, 'other order', 'other ticket')
        self.boundaries = {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                           for p in (self.control, self.other)}
        fake = self.root / 'bin/docker'
        fake.write_text('#!' + sys.executable + '\n' + '''
from pathlib import Path
import json, os, shutil, subprocess, sys
root = Path(os.environ['FIXTURE_ROOT'])
args = sys.argv[1:]
if args[:2] == ['image', 'inspect']:
    with (root / 'calls.jsonl').open('a') as log:
        log.write(json.dumps(args) + '\\n')
    sys.exit(1 if args[2] in os.environ.get('MISSING_IMAGES', '').split() else 0)
assert args[0] == 'compose'
context = []
while args[1] in ('--env-file', '-f'):
    context += args[1:3]
    del args[1:3]
assert os.getcwd() == str(root)
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\\n')
with (root / 'contexts.jsonl').open('a') as log:
    log.write(json.dumps(context) + '\\n')
if args[1] == 'config':
    assert args[2:] == ['--images', 'app', 'litestream']
    print(os.environ['FIXTURE_IMAGES'])
    sys.exit(int(os.environ.get('CONFIG_EXIT', '0')))
if args[1] in ('stop', 'start'):
    assert args[2:] == ['app', 'litestream']
    sys.exit(1 if args[1] == 'start' and os.environ.get('START_FAIL') == '1' else 0)
assert args[1] == 'run'
if 'litestream' in args:
    assert '-config' in args and '/etc/litestream-restore.yml' in args
    assert 'ORGANIZER_ID=' + os.environ['FIXTURE_ORGANIZER'] in args
    if '-dry-run' in args:
        sys.exit(0)
    assert args[args.index('-integrity-check') + 1] == 'full'
    if os.environ.get('RESTORE_FAIL') == '1':
        (root / args[args.index('-o') + 1].removeprefix('/')).write_bytes(b'partial restore')
        sys.exit(1)
    out = args[args.index('-o') + 1]
    shutil.copyfile(root / 'snapshot.db', root / out.removeprefix('/'))
    if os.environ.get('RESTORE_SYMLINK') == '1':
        path = root / out.removeprefix('/')
        path.unlink()
        path.symlink_to(root / 'data/organizers/other.db')
    if os.environ.get('RESTORE_SIDECAR') == '1':
        (root / (out + '-wal').removeprefix('/')).write_bytes(b'unexpected restore WAL')
    if os.environ.get('CHANGE_PARENT') == '1':
        parent = root / 'data/organizers'
        parent.rename(root / 'data/retained-parent')
        parent.mkdir()
        for p in (root / 'data/retained-parent').iterdir():
            shutil.move(str(p), str(parent / p.name))
    sys.exit(0)
assert 'app' in args and '--entrypoint' in args
command = args[args.index('-c') + 1:]
command[2:] = [str(root / a.removeprefix('/')) if a.startswith('/data/') else a
               for a in command[2:]]
sys.exit(subprocess.run(['/bin/sh', '-c', *command]).returncode)
''')
        fake.chmod(0o755)
        for name in ('chmod', 'chown'):
            wrapper = self.root / 'bin' / name
            wrapper.write_text('#!' + sys.executable + '\n' + """
import os, subprocess, sys
if sys.argv[1] == '--reference':
    stat = os.stat(sys.argv[2])
    if os.path.basename(sys.argv[0]) == 'chmod':
        os.chmod(sys.argv[3], stat.st_mode & 0o7777)
    else:
        os.chown(sys.argv[3], stat.st_uid, stat.st_gid)
else:
    sys.exit(subprocess.run(['/bin/' + os.path.basename(sys.argv[0]), *sys.argv[1:]]).returncode)
""")
            wrapper.chmod(0o755)
        wrapper = self.root / 'bin/stat'
        wrapper.write_text('#!' + sys.executable + '\n' + """
import os, sys
assert sys.argv[1] == '-c'
s = os.stat(sys.argv[3])
print({'%h': str(s.st_nlink), '%d:%i': str(s.st_dev) + ':' + str(s.st_ino)}[sys.argv[2]])
""")
        wrapper.chmod(0o755)
        self.env = {'PATH': str(self.root / 'bin') + ':/usr/bin:/bin',
                    'CRABGATE_INSTALL_DIR': str(self.root), 'FIXTURE_ROOT': str(self.root),
                    'FIXTURE_ORGANIZER': ORGANIZER,
                    'FIXTURE_IMAGES': LITESTREAM_IMAGE + '\ncrabgate:local'}
        self.script = Path(__file__).resolve().parents[1] / 'scripts/restore-organizer.sh'

    def create(self, path, order, ticket):
        with closing(sqlite3.connect(path)) as db, db:
            db.executescript('CREATE TABLE orders (value TEXT); CREATE TABLE tickets (value TEXT);')
            db.execute('INSERT INTO orders VALUES (?)', (order,))
            db.execute('INSERT INTO tickets VALUES (?)', (ticket,))

    def values(self, path):
        with closing(sqlite3.connect('file:' + str(path) + '?mode=ro', uri=True)) as db:
            self.assertEqual(db.execute('PRAGMA integrity_check').fetchone()[0], 'ok')
            return (db.execute('SELECT value FROM orders').fetchall(),
                    db.execute('SELECT value FROM tickets').fetchall())

    def run_script(self, args, apply=False, **extra):
        if not apply:
            result = subprocess.run(['sh', str(self.script), *args], env=dict(self.env, **extra),
                                    text=True, capture_output=True)
            output = result.stdout + result.stderr
        else:
            master, slave = pty.openpty()
            def controlling_terminal():
                os.setsid()
                fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
            process = subprocess.Popen(['sh', str(self.script), *args],
                                       env=dict(self.env, **extra), stdin=slave, stdout=slave,
                                       stderr=slave, preexec_fn=controlling_terminal)
            os.close(slave)
            os.write(master, b'RESTORE\n')
            chunks = []
            while True:
                try:
                    chunk = os.read(master, 4096)
                except OSError:
                    break
                if not chunk:
                    break
                chunks.append(chunk)
            os.close(master)
            process.wait(timeout=10)
            result = process
            output = b''.join(chunks).decode()
        (self.root / 'script-output.log').write_text(output)
        return result.returncode, output

    def assert_boundaries(self):
        for name, digest in self.boundaries.items():
            self.assertEqual(hashlib.sha256(Path(name).read_bytes()).hexdigest(), digest)

    def test_apply_preserves_wal_recovery_copy_and_permissions(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertEqual(status, 0, output)
        backups = list(self.target.parent.glob(ORGANIZER + '.db.pre-restore-*'))
        backups = [p for p in backups if not p.name.endswith(('-wal', '-shm'))]
        self.assertEqual(len(backups), 1)
        self.assertEqual(self.values(backups[0]), ([('original WAL order',)], [('original WAL ticket',)]))
        self.assertEqual(self.values(self.target), ([('restored order',)], [('restored ticket',)]))
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o600)
        self.assertFalse(Path(str(self.target) + '-wal').exists())
        self.assertFalse(Path(str(self.target) + '-shm').exists())
        self.assert_boundaries()

    def test_replacement_preserves_original_access_mode(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertEqual(status, 0, output)
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o600)

    def test_failure_keeps_original_and_services_stopped(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True, RESTORE_FAIL='1')
        self.assertEqual(status, 1, output)
        self.assertIn('failed during remote-restore', output)
        self.assertEqual(Path(str(self.target) + '.restore').read_bytes(), b'partial restore')
        self.assertEqual(self.values(self.target), ([('original WAL order',)], [('original WAL ticket',)]))
        calls = [json.loads(line) for line in (self.root / 'calls.jsonl').read_text().splitlines()]
        self.assertNotIn(['compose', 'start', 'app', 'litestream'], calls)
        self.assert_boundaries()

    def test_dry_run_never_stops_or_changes_files(self):
        before = {p.name: p.read_bytes() for p in self.target.parent.iterdir()}
        status, output = self.run_script([ORGANIZER, TIMESTAMP])
        self.assertEqual(status, 0, output)
        self.assertEqual({p.name: p.read_bytes() for p in self.target.parent.iterdir()}, before)
        calls = self.calls()
        self.assertEqual(calls[:3], PREFLIGHT + [['image', 'inspect', LITESTREAM_IMAGE],
                                                 ['image', 'inspect', 'crabgate:local']])
        self.assertEqual(len(calls), 4)
        self.assertIn('-dry-run', calls[3])
        self.assertEqual(self.contexts(), [[], []])
        self.assertIn('Compose context: Docker Compose defaults in ' + str(self.root) + ' (no .deploy.env).', output)
        self.assert_boundaries()

    def test_invalid_ids_do_not_reach_docker(self):
        for identifier in ('', '-', 'deadbeef', '../control', ORGANIZER + '0', '--------' + ORGANIZER[8:]):
            status, _ = self.run_script([identifier, TIMESTAMP])
            self.assertEqual(status, 2, identifier)
        self.assertFalse((self.root / 'calls.jsonl').exists())

    def calls(self):
        return [json.loads(line) for line in (self.root / 'calls.jsonl').read_text().splitlines()]

    def contexts(self):
        return [json.loads(line) for line in (self.root / 'contexts.jsonl').read_text().splitlines()]

    def deployed(self, image=DEPLOYED_IMAGE):
        (self.root / '.env').write_text('FIXTURE_SETTING=sanitized\n')
        (self.root / '.deploy.env').write_text('CRABGATE_IMAGE=' + image + '\n')
        for name in ('compose.yaml', 'compose.hosted.yaml'):
            (self.root / name).write_text('services: {}\n')
        self.env['FIXTURE_IMAGES'] = image + '\n' + LITESTREAM_IMAGE

    def test_apply_in_deployed_context_uses_deployed_image_env_and_compose_files(self):
        self.deployed()
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertEqual(status, 0, output)
        self.assertIn('Compose context: deployed image ' + DEPLOYED_IMAGE
                      + ' with .env, .deploy.env, compose.yaml and compose.hosted.yaml in ' + str(self.root) + '.', output)
        calls = self.calls()
        self.assertEqual(calls[:4], PREFLIGHT + [['image', 'inspect', DEPLOYED_IMAGE],
                                                 ['image', 'inspect', LITESTREAM_IMAGE],
                                                 ['compose', 'stop', 'app', 'litestream']])
        self.assertEqual([call[1] for call in calls if call[0] == 'compose'],
                         ['config', 'stop', 'run', 'run', 'run', 'start'])
        self.assertEqual(self.contexts(), [DEPLOYED_CONTEXT] * 6)
        self.assertEqual(self.values(self.target), ([('restored order',)], [('restored ticket',)]))
        self.recovery()
        self.assert_boundaries()

    def test_dry_run_in_deployed_context_uses_the_same_context(self):
        self.deployed()
        status, output = self.run_script([ORGANIZER, TIMESTAMP])
        self.assertEqual(status, 0, output)
        self.assertEqual(self.contexts(), [DEPLOYED_CONTEXT] * 2)
        self.assertEqual([call[1] for call in self.calls() if call[0] == 'compose'], ['config', 'run'])

    def test_apply_without_deploy_record_uses_compose_defaults(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertEqual(status, 0, output)
        self.assertEqual(self.contexts(), [[]] * 6)
        self.assertEqual(self.calls()[:4], PREFLIGHT + [['image', 'inspect', LITESTREAM_IMAGE],
                                                        ['image', 'inspect', 'crabgate:local'],
                                                        ['compose', 'stop', 'app', 'litestream']])

    def assert_refused_before_stop(self, t, status, output, message):
        self.assertEqual(status, 1, output)
        self.assertIn(message + ' Nothing was changed.', output)
        self.assertNotIn('Type RESTORE', output)
        self.assertNotIn('Restore failed during', output)
        calls = t.calls() if (t.root / 'calls.jsonl').exists() else []
        self.assertEqual([call for call in calls if call[:2] != ['image', 'inspect'] and call != PREFLIGHT[0]], [])
        self.assertEqual(sorted(path.name for path in t.target.parent.iterdir()),
                         sorted([ORGANIZER + '.db', ORGANIZER + '.db-shm', ORGANIZER + '.db-wal', 'other.db']))
        self.assertEqual(t.values(t.target), ([('original WAL order',)], [('original WAL ticket',)]))
        t.assert_boundaries()

    def test_unusable_image_or_configuration_stops_before_services(self):
        cases = {
            'deployed image absent': (dict(MISSING_IMAGES=DEPLOYED_IMAGE),
                                      'Image ' + DEPLOYED_IMAGE + ' is not available to Docker on this server.'),
            'litestream image absent': (dict(MISSING_IMAGES=LITESTREAM_IMAGE),
                                        'Image ' + LITESTREAM_IMAGE + ' is not available to Docker on this server.'),
            'configuration invalid': (dict(CONFIG_EXIT='1'), 'Docker Compose cannot resolve this configuration.'),
            'no image resolved': (dict(FIXTURE_IMAGES=''), 'Docker Compose resolved no app or litestream image.'),
            'other app image': (dict(FIXTURE_IMAGES='crabgate:local\n' + LITESTREAM_IMAGE),
                                'The Compose configuration does not run the deployed image ' + DEPLOYED_IMAGE + '.'),
        }
        for name, (extra, message) in cases.items():
            for mode in (['--apply'], []):
                with self.subTest(name=name, mode=mode):
                    t = OrganizerRestore('test_failure_keeps_original_and_services_stopped')
                    t.setUp()
                    self.addCleanup(t.doCleanups)
                    t.deployed()
                    status, output = t.run_script([ORGANIZER, TIMESTAMP, *mode], apply=bool(mode), **extra)
                    self.assert_refused_before_stop(t, status, output, message)

    def test_incomplete_deployed_context_stops_before_docker(self):
        for name in ('.env', 'compose.yaml', 'compose.hosted.yaml', 'image'):
            with self.subTest(name=name):
                t = OrganizerRestore('test_failure_keeps_original_and_services_stopped')
                t.setUp()
                self.addCleanup(t.doCleanups)
                t.deployed()
                if name == 'image':
                    (t.root / '.deploy.env').write_text('OTHER=value\n')
                    message = str(t.root) + '/.deploy.env does not name CRABGATE_IMAGE.'
                else:
                    (t.root / name).unlink()
                    message = 'Deployed Compose context is incomplete: ' + str(t.root) + '/' + name + ' is not readable.'
                status, output = t.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
                self.assert_refused_before_stop(t, status, output, message)
                self.assertFalse((t.root / 'calls.jsonl').exists())

    def test_local_default_image_absent_stops_before_services(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True, MISSING_IMAGES='crabgate:local')
        self.assert_refused_before_stop(self, status, output,
                                        'Image crabgate:local is not available to Docker on this server.')

    def assert_stopped(self):
        self.assertNotIn(['compose', 'start', 'app', 'litestream'], self.calls())
        self.assert_boundaries()

    def recovery(self):
        paths = [p for p in self.target.parent.glob(ORGANIZER + '.db.pre-restore-*')
                 if not p.name.endswith(('-wal', '-shm'))]
        self.assertEqual(len(paths), 1)
        self.assertEqual(self.values(paths[0]), ([('original WAL order',)], [('original WAL ticket',)]))
        return paths[0]

    def recover_original_offline(self):
        backup = self.recovery()
        stage = self.root / 'verified-recovery'
        evidence = self.root / 'failed-live-evidence'
        stage.mkdir()
        evidence.mkdir()
        staged = stage / self.target.name
        command = 'set -eu; for suffix in "" -wal -shm; do if test -f "$1$suffix"; then cp -p "$1$suffix" "$2$suffix"; fi; done'
        result = subprocess.run(['sh', '-c', command, '_', str(backup), str(staged)],
                                env=dict(self.env, PATH="/usr/bin:/bin"), text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.values(staged), ([('original WAL order',)], [('original WAL ticket',)]))
        command = 'set -eu; for suffix in "" -wal -shm; do if test -f "$1$suffix"; then mv "$1$suffix" "$3/$4$suffix"; fi; done; for suffix in -wal -shm ""; do if test -f "$2$suffix"; then mv "$2$suffix" "$1$suffix"; fi; done'
        result = subprocess.run(['sh', '-c', command, '_', str(self.target), str(staged),
                                 str(evidence), self.target.name], env=dict(self.env, PATH="/usr/bin:/bin"), text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.values(self.target), ([('original WAL order',)], [('original WAL ticket',)]))
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o600)

    def fault(self, name, body):
        wrapper = self.root / 'bin' / name
        wrapper.write_text('#!/bin/sh\n' + body + '\nexec /bin/' + name + ' "$@"\n')
        wrapper.chmod(0o755)

    def test_cleanup_failure_leaves_coherent_restored_database(self):
        self.fault('rm', 'case "$*" in *retired-wal*) echo "synthetic cleanup failure" >&2; exit 1;; esac')
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertEqual(status, 1, output)
        self.assertIn('Restore stage: installed', output)
        self.assertIn('failed during installation', output)
        self.assertEqual(self.values(self.target), ([('restored order',)], [('restored ticket',)]))
        self.assertFalse(Path(str(self.target) + '-wal').exists())
        self.recovery()
        self.assert_stopped()
        self.recover_original_offline()

    def test_interruption_after_install_before_cleanup(self):
        self.fault('mv', 'case "$1" in *.db.restore) /bin/mv "$@"; kill -TERM "$PPID"; exit 0;; esac')
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertNotEqual(status, 0, output)
        self.assertIn('failed during installation', output)
        work = Path(str(self.target) + '.restore-work')
        self.assertEqual((work / 'stage').read_text().strip(), 'installing')
        self.assertFalse(Path(str(self.target) + '.restore').exists())
        self.assertEqual(self.values(self.target), ([('restored order',)], [('restored ticket',)]))
        self.recovery()
        self.assert_stopped()
        self.recover_original_offline()

    def test_detachment_and_install_failure_preserve_complete_recovery(self):
        for failing in ('-wal', '-shm', '.restore'):
            with self.subTest(failing=failing):
                t = OrganizerRestore('test_failure_keeps_original_and_services_stopped')
                t.setUp()
                self.addCleanup(t.doCleanups)
                t.fault('mv', 'case "$1" in *' + failing + ') echo "synthetic move failure" >&2; exit 1;; esac')
                status, output = t.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
                self.assertEqual(status, 1, output)
                self.assertIn('failed during installation', output)
                t.recovery()
                self.assertTrue(Path(str(t.target) + '.restore').exists())
                t.assert_stopped()
                t.recover_original_offline()

    def test_recovery_copy_failure_does_not_claim_complete_set(self):
        self.fault('cp', 'case "$*" in *.pre-restore-*-wal*) echo "synthetic copy failure" >&2; exit 1;; esac')
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
        self.assertEqual(status, 1, output)
        self.assertIn('failed during preparation', output)
        work = Path(str(self.target) + '.restore-work')
        self.assertFalse((work / 'recovery-complete').exists())
        self.assertEqual(self.values(self.target), ([('original WAL order',)], [('original WAL ticket',)]))
        self.assertFalse(any('litestream' in c and 'restore' in c for c in self.calls()))
        self.assert_stopped()

    def test_parent_identity_change_fails_before_install(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True, CHANGE_PARENT='1')
        self.assertEqual(status, 1, output)
        self.assertIn('Data parent identity changed', output)
        self.assertEqual(self.values(self.target), ([('original WAL order',)], [('original WAL ticket',)]))
        self.recovery()
        self.assert_stopped()

    def test_unsafe_paths_fail_closed_without_recovery_copy(self):
        for kind in ('target', 'wal', 'shm', 'parent', 'data', 'restore', 'restore-wal',
                     'restore-shm', 'work', 'backup', 'backup-wal', 'backup-shm', 'hardlink', 'dangling-wal'):
            with self.subTest(kind=kind):
                t = OrganizerRestore('test_failure_keeps_original_and_services_stopped')
                t.setUp()
                self.addCleanup(t.doCleanups)
                if kind in ('parent', 'data'):
                    path = t.target.parent if kind == 'parent' else t.root / 'data'
                    retained = t.root / ('retained-' + kind)
                    path.rename(retained)
                    path.symlink_to(retained, target_is_directory=True)
                elif kind == 'hardlink':
                    t.target.unlink()
                    os.link(t.other, t.target)
                else:
                    suffix = {'target': '', 'wal': '-wal', 'shm': '-shm', 'restore': '.restore',
                              'restore-wal': '.restore-wal', 'restore-shm': '.restore-shm',
                              'work': '.restore-work', 'backup': '.pre-restore-fixed',
                              'backup-wal': '.pre-restore-fixed-wal', 'backup-shm': '.pre-restore-fixed-shm',
                              'dangling-wal': '-wal'}[kind]
                    path = Path(str(t.target) + suffix)
                    if path.exists():
                        path.unlink()
                    path.symlink_to(t.root / 'absent' if kind == 'dangling-wal' else t.other)
                    if kind.startswith('backup'):
                        t.fault('date', 'echo fixed; exit 0')
                status, output = t.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
                self.assertEqual(status, 1, output)
                self.assertFalse(any('litestream' in c and 'restore' in c for c in t.calls()))
                self.assertFalse(Path(str(t.target) + '.restore-work/recovery-complete').exists())
                self.assertEqual(t.values(t.other), ([('other order',)], [('other ticket',)]))
                t.assert_stopped()

    def test_existing_restore_and_workspace_are_retained(self):
        for suffix in ('.restore', '.restore-work'):
            with self.subTest(suffix=suffix):
                t = OrganizerRestore('test_failure_keeps_original_and_services_stopped')
                t.setUp()
                self.addCleanup(t.doCleanups)
                path = Path(str(t.target) + suffix)
                if suffix.endswith('work'):
                    path.mkdir()
                    evidence = path / 'evidence'
                else:
                    evidence = path
                evidence.write_bytes(b'prior recovery evidence')
                status, output = t.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True)
                self.assertEqual(status, 1, output)
                self.assertEqual(evidence.read_bytes(), b'prior recovery evidence')
                self.assertEqual(t.values(t.target), ([('original WAL order',)], [('original WAL ticket',)]))
                t.assert_stopped()

    def test_unsafe_remote_output_leaves_original_set(self):
        for flag in ('RESTORE_SYMLINK', 'RESTORE_SIDECAR'):
            with self.subTest(flag=flag):
                t = OrganizerRestore('test_failure_keeps_original_and_services_stopped')
                t.setUp()
                self.addCleanup(t.doCleanups)
                status, output = t.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True, **{flag: '1'})
                self.assertEqual(status, 1, output)
                self.assertIn('failed during installation', output)
                self.assertEqual(t.values(t.target), ([('original WAL order',)], [('original WAL ticket',)]))
                t.recovery()
                t.assert_stopped()

    def test_failed_start_stops_both_services_again(self):
        status, output = self.run_script([ORGANIZER, TIMESTAMP, '--apply'], apply=True, START_FAIL='1')
        self.assertEqual(status, 1, output)
        self.assertIn('failed during service-start', output)
        self.assertEqual(self.calls()[-1], ['compose', 'stop', 'app', 'litestream'])
        self.assertEqual(self.values(self.target), ([('restored order',)], [('restored ticket',)]))
        self.recovery()
        self.assert_boundaries()


if __name__ == '__main__':
    unittest.main()
