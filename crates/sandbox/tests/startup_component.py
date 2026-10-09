#!/usr/bin/env python3
"""Real runsc startup-protocol component test, only inside a disposable VM.

Grants are explicit protocol fixtures, not database authority. No Candidate CSI
mount, Kubernetes attach, credential transport, or physical fence is certified.
"""
import argparse
import hashlib
import json
import os
import pathlib
import select
import subprocess
import time
import uuid

p = argparse.ArgumentParser()
p.add_argument('--runsc', type=pathlib.Path, required=True)
p.add_argument('--rootfs', type=pathlib.Path, required=True)
p.add_argument('--work-dir', type=pathlib.Path, required=True)
a = p.parse_args()
assert os.geteuid() == 0, 'requires a disposable root-controlled VM'
a.work_dir.mkdir(parents=True, exist_ok=False)
runtime = [str(a.runsc), '--root=' + str(a.work_dir / 'state'), '--platform=systrap',
           '--network=none', '--ignore-cgroups=true', '--directfs=false']
records = []


def digest(domain, value):
    data = json.dumps(value, separators=(',', ':'), ensure_ascii=False).encode()
    return 'sha256:' + hashlib.sha256(domain.encode() + b'\0' + data).hexdigest()


def execute(name, *, delay=0, budget=5000, ceiling=10000, invalid=None,
            expected_error=None, argv=None, outcome='succeeded'):
    case = a.work_dir / name
    case.mkdir()
    workspace = case / 'workspace'
    workspace.mkdir(mode=0o700)
    os.chown(workspace, 1000, 1000)
    request = dict(execution_id=name, generation=7,
                   argv=argv or ['/bin/sh', '-c', 'printf started > result; printf literal'],
                   cwd='', timeout_seconds=10, lease_budget_ms=ceiling,
                   term_grace_ms=100, output_limit_bytes=4096)
    bootstrap = dict(version=1, intent_digest='sha256:' + 'a' * 64, request=request)
    (case / 'bootstrap.json').write_text(json.dumps(bootstrap))
    config = {
        'ociVersion': '1.0.2', 'hostname': 'startup-component',
        'root': {'path': str(a.rootfs), 'readonly': True},
        'process': {'terminal': False, 'user': {'uid': 1000, 'gid': 1000},
                    'args': ['/bin/agent-computer-sandbox', '--startup', '/request.json'],
                    'env': ['AC_HOST_SECRET=must-not-leak'], 'cwd': '/', 'noNewPrivileges': True,
                    'capabilities': {k: [] for k in ['bounding', 'effective', 'inheritable', 'permitted', 'ambient']},
                    'rlimits': [{'type': 'RLIMIT_NOFILE', 'hard': 128, 'soft': 128},
                                {'type': 'RLIMIT_NPROC', 'hard': 64, 'soft': 64}]},
        'mounts': [
            {'destination': '/proc', 'type': 'proc', 'source': 'proc', 'options': ['nosuid', 'noexec', 'nodev']},
            {'destination': '/dev', 'type': 'tmpfs', 'source': 'tmpfs', 'options': ['nosuid', 'mode=755', 'size=1m']},
            {'destination': '/tmp', 'type': 'tmpfs', 'source': 'tmpfs', 'options': ['nosuid', 'nodev', 'mode=1777', 'size=64m']},
            {'destination': '/workspace', 'type': 'bind', 'source': str(workspace), 'options': ['rbind', 'rw', 'nosuid', 'nodev']},
            {'destination': '/request.json', 'type': 'bind', 'source': str(case / 'bootstrap.json'), 'options': ['bind', 'ro', 'nosuid', 'nodev']}],
        'linux': {'namespaces': [{'type': n} for n in ['pid', 'network', 'ipc', 'uts', 'mount']],
                  'maskedPaths': ['/proc/kcore', '/proc/keys', '/proc/timer_list', '/sys/firmware'],
                  'readonlyPaths': ['/proc/sys', '/proc/sysrq-trigger']}}
    (case / 'config.json').write_text(json.dumps(config))
    container = 'ac-startup-' + uuid.uuid4().hex
    start = time.monotonic()
    with (case / 'stderr.log').open('w') as stderr:
        process = subprocess.Popen(runtime + ['run', '--bundle=' + str(case), container],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr)
        try:
            assert select.select([process.stdout], [], [], 15)[0], 'challenge not received'
            line = process.stdout.readline(65537)
            challenge = json.loads(line)
            assert challenge['execution_id'] == name and challenge['generation'] == 7
            assert challenge['bootstrap_digest'] == digest('agent-computer/sandbox-bootstrap-v1', bootstrap)
            assert len(challenge['nonce']) == 64 and challenge['nonce'] not in [r['challenge']['nonce'] for r in records]
            assert not (workspace / 'result').exists(), 'command started before a grant'
            grant = dict(version=1, challenge_digest=digest('agent-computer/sandbox-challenge-v1', challenge),
                         lease_budget_ms=budget)
            if delay:
                time.sleep(delay)
                assert not (workspace / 'result').exists(), 'command ran while authorization was pending'
            if invalid == 'digest':
                grant['challenge_digest'] = 'sha256:' + '0' * 64
            frame = json.dumps(grant, separators=(',', ':')).encode() + b'\n'
            if invalid == 'two-frames':
                frame += frame
            if invalid == 'oversized':
                frame = b' ' * 65537
            if invalid == 'cancel':
                subprocess.run(runtime + ['kill', container, 'TERM'], check=True, capture_output=True, timeout=5)
            elif invalid == 'eof':
                process.stdin.close()
            elif invalid != 'withhold':
                if invalid == 'fragmented':
                    process.stdin.write(frame[:11])
                    process.stdin.flush()
                    time.sleep(0.02)
                    frame = frame[11:]
                process.stdin.write(frame)
                process.stdin.flush()
            code = process.wait(timeout=40)
            remaining = process.stdout.read()
        finally:
            subprocess.run(runtime + ['delete', '--force', container], check=True, capture_output=True, timeout=10)
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            if not process.stdin.closed:
                process.stdin.close()
            process.stdout.close()
    wall_ms = round((time.monotonic() - start) * 1000)
    (case / 'stdout.jsonl').write_bytes(line + remaining)
    record = dict(name=name, bootstrap=bootstrap, challenge=challenge, fixture_grant=grant,
                  exit_code=code, wall_ms=wall_ms)
    if expected_error:
        assert code == 125, (name, code, (case / 'stderr.log').read_text())
        assert expected_error in (case / 'stderr.log').read_text(), (name, (case / 'stderr.log').read_text())
        assert not remaining, (name, remaining)
        assert not (workspace / 'result').exists(), 'rejected startup changed workspace'
        record.update(rejected=True, error=expected_error, command_started=False)
    else:
        assert code == 0, (name, code, (case / 'stderr.log').read_text())
        report = json.loads(remaining)
        assert report['challenge_digest'] == grant['challenge_digest']
        assert report['grant_digest'] == digest('agent-computer/sandbox-startup-grant-v1', grant)
        assert report['report']['outcome'] == outcome, report
        assert report['report']['children_reaped'], report
        record['report'] = report
    records.append(record)
    return record


r = execute('valid')
assert bytes(r['report']['report']['stdout']['bytes']) == b'literal'
r = execute('delayed-valid', delay=0.25, invalid='fragmented')
assert r['report']['report']['elapsed_ms'] >= 250
execute('late-grant', delay=0.25, budget=100, expected_error='StartupExpired')
execute('wrong-challenge', invalid='digest', expected_error='InvalidRequest')
execute('budget-increase', budget=1000, ceiling=500, expected_error='InvalidRequest')
execute('closed-channel', invalid='eof', expected_error='InvalidRequest')
execute('cancel-before-grant', invalid='cancel', expected_error='StartupCancelled')
execute('duplicate-frame', invalid='two-frames', expected_error='InvalidRequest')
execute('oversized-frame', invalid='oversized', expected_error='InvalidRequest')
r = execute('anchored-deadline', delay=0.2, budget=500,
            argv=['/bin/sh', '-c', "trap '' TERM; printf started > result; while :; do /bin/sleep 10; done"], outcome='lease_expired')
assert 500 <= r['report']['report']['elapsed_ms'] < 2500
assert r['report']['report']['kill_sent']
r = execute('stdin-and-environment', argv=['/bin/sh', '-c', '/bin/cat; /bin/env'])
# The explicitly invoked shell adds PWD; it must not inherit the fixture secret
# or the startup frame, and cat must observe EOF on the child's null stdin.
assert sorted(bytes(r['report']['report']['stdout']['bytes']).decode().splitlines()) == ['HOME=/tmp', 'LANG=C', 'PATH=/usr/bin:/bin', 'PWD=/workspace']
execute('withheld-grant', invalid='withhold', expected_error='StartupExpired')
version = subprocess.run([str(a.runsc), '--version'], check=True, capture_output=True, text=True).stdout.strip()
result = dict(component='sandbox-startup-handshake', runtime=version, platform='systrap', network='none',
              tests=records, grants_are_protocol_fixtures=True, product_execution_admission=False,
              kubernetes_attach_verified=False, candidate_mount_verified=False, physical_fencing=False,
              cgroup_limits_verified=False, accepted_runtime_tests=[],
              fixture_files=json.loads((a.rootfs / 'fixture-files.json').read_text()),
              test_script_sha256=hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest())
(a.work_dir / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(dict(passed=len(records), result=str(a.work_dir / 'result.json'))))
