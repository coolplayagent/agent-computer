#!/usr/bin/env python3
"""Disposable root systemd VM only. Exercises the committed expiry service."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import time
import uuid

UNIT = 'agent-computer-expiry-reaper.service'
SPOOL = Path('/var/lib/agent-computer/watchdogs')


def now():
    return time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1_000_000


def ctl(*args):
    return subprocess.check_output(['systemctl', *args], text=True, timeout=25).strip()


def until(predicate, seconds=5):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.03)
    raise AssertionError('condition timed out')


def frame(pipe):
    assert select.select([pipe], [], [], 3)[0]
    return json.loads(pipe.readline())


def live(group):
    return 'populated 1\n' in (group / 'cgroup.events').read_text()


def once(binary):
    value = subprocess.run([str(binary), '--reap-once', '--spool', str(SPOOL)], capture_output=True, text=True, timeout=5)
    return value.returncode, [entry for line in value.stdout.splitlines() for entry in json.loads(line)['entries']]


def rewrite_intent(directory, change):
    path = directory / 'intent.json'
    value = json.loads(path.read_bytes())
    change(value['request'])
    data = json.dumps(value).encode()
    path.write_bytes(data)
    enrollment_path = directory / 'enrollment.json'
    enrollment = json.loads(enrollment_path.read_bytes())
    enrollment['journal']['intent_digest'] = 'sha256:' + hashlib.sha256(data).hexdigest()
    enrollment_path.write_text(json.dumps(enrollment))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--watchdog', type=Path, required=True)
    parser.add_argument('--unit', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    assert os.geteuid() == 0 and Path('/run/systemd/system').is_dir()
    installed = Path('/usr/local/bin/agent-computer-watchdog')
    unit = Path('/etc/systemd/system') / UNIT
    assert not installed.exists() and not unit.exists() and not SPOOL.exists(), 'requires a fresh disposable fixture VM'
    shutil.copyfile(args.watchdog, installed)
    installed.chmod(0o755)
    shutil.copyfile(args.unit, unit)
    unit.chmod(0o644)
    subprocess.run(['systemd-analyze', 'verify', str(unit)], check=True, timeout=10)
    ctl('daemon-reload')
    results = []
    cases = ['both-killed', 'both-stopped', 'service-killed', 'service-stopped', 'interrupted-publication',
             'wrong-device', 'wrong-inode', 'old-boot', 'altered-digest', 'unenrolled', 'path-reuse', 'pagination']
    try:
        ctl('start', UNIT)
        assert SPOOL.stat().st_mode & 0o777 == 0o700
        code, _ = once(installed)
        assert code == 2, 'second reaper must not acquire the directory lock'
        results.append({'case': 'exclusive-reaper', 'status': 'pass'})
        for case in cases:
            group = Path('/sys/fs/cgroup') / ('ac-reaper-' + uuid.uuid4().hex)
            group.mkdir()
            processes, guards, journals = [], [], []
            try:
                child = subprocess.Popen(['/bin/sleep', '60'])
                processes.append(child)
                (group / 'cgroup.procs').write_text(str(child.pid))
                child.send_signal(signal.SIGSTOP)
                request = {'version': 1, 'execution_id': case,
                           'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                           'cgroup_path': group.name, 'cgroup_inode': group.stat().st_ino,
                           'deadline_boottime_ms': now() + 1800}
                source = SPOOL / 'request.json'
                source.write_text(json.dumps(request))
                for index in range(2):
                    journal = SPOOL / ('journal-' + uuid.uuid4().hex)
                    journal.mkdir(mode=0o700)
                    journals.append(journal)
                    guard = subprocess.Popen([str(installed), '--request', str(source), '--journal', str(journal)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    guards.append(guard)
                    receipt = frame(guard.stdout)
                    assert receipt['request'] == request
                    enrollment = json.loads((journal / 'enrollment.json').read_bytes())
                    assert enrollment['journal'] == receipt['journal'] and enrollment['cgroup_device'] == receipt['cgroup_device']
                for guard in guards:
                    guard.send_signal(signal.SIGSTOP if case == 'both-stopped' else signal.SIGKILL)
                    if case != 'both-stopped':
                        guard.wait(timeout=3)
                    guard.stdout.close()
                source.unlink()
                if case in ['service-killed', 'service-stopped']:
                    previous = int(ctl('show', UNIT, '--property=MainPID', '--value'))
                    assert previous > 1
                    os.kill(previous, signal.SIGKILL if case == 'service-killed' else signal.SIGSTOP)
                if case == 'interrupted-publication':
                    for journal in journals:
                        (journal / 'recovery-123-1-0.pending').write_bytes(b'{')
                elif case == 'wrong-device':
                    for journal in journals:
                        path = journal / 'enrollment.json'
                        value = json.loads(path.read_bytes())
                        value['cgroup_device'] += 1
                        path.write_text(json.dumps(value))
                elif case == 'wrong-inode':
                    for journal in journals:
                        rewrite_intent(journal, lambda value: value.update(cgroup_inode=value['cgroup_inode'] + 1))
                elif case == 'old-boot':
                    for journal in journals:
                        rewrite_intent(journal, lambda value: value.update(boot_id='12345678-1234-1234-1234-123456789abc'))
                elif case == 'altered-digest':
                    for journal in journals:
                        with (journal / 'intent.json').open('ab') as file:
                            file.write(b' ')
                elif case == 'unenrolled':
                    for journal in journals:
                        (journal / 'enrollment.json').unlink()
                elif case == 'path-reuse':
                    (group / 'cgroup.kill').write_text('1')
                    child.wait(timeout=3)
                    group.rmdir()
                    group.mkdir()
                    assert group.stat().st_ino != request['cgroup_inode']
                    replacement = subprocess.Popen(['/bin/sleep', '60'])
                    processes.append(replacement)
                    (group / 'cgroup.procs').write_text(str(replacement.pid))
                elif case == 'pagination':
                    for i in range(300):
                        (SPOOL / ('journal-empty-' + str(i))).mkdir(mode=0o700)
                rejected = case in ['wrong-device', 'wrong-inode', 'old-boot', 'altered-digest', 'unenrolled', 'path-reuse']
                if rejected:
                    until(lambda: now() > request['deadline_boottime_ms'] + 600)
                    assert live(group), case
                    ctl('stop', UNIT)
                    code, entries = once(installed)
                    expected = {'old-boot': 'historical_boot', 'unenrolled': 'unenrolled'}.get(case, 'unavailable')
                    assert len(entries) == 2 and all(v['outcome']['state'] == expected for v in entries), entries
                    assert code == (0 if expected != 'unavailable' else 2)
                    assert live(group)
                    results.append({'case': case, 'status': 'pass', 'target_untouched': True, 'outcomes': entries})
                    ctl('start', UNIT)
                else:
                    until(lambda: all((p / 'recovery.json').exists() for p in journals), 18 if case == 'service-stopped' else 6)
                    assert not live(group)
                    reports = [json.loads((p / 'recovery.json').read_bytes()) for p in journals]
                    assert all(v['trigger'] == 'Recovery' and v['observation'] == 'EmptyObserved' and v['error'] is None for v in reports)
                    assert all(v['request'] == request and v['kill_boottime_ms'] >= request['deadline_boottime_ms'] for v in reports)
                    assert all(not (p / 'report.json').exists() for p in journals)
                    restarts = int(ctl('show', UNIT, '--property=NRestarts', '--value'))
                    if case in ['service-killed', 'service-stopped']:
                        assert int(ctl('show', UNIT, '--property=MainPID', '--value')) != previous
                        assert restarts >= 1
                    ctl('stop', UNIT)
                    before = [(p / 'recovery.json').read_bytes() for p in journals]
                    code, entries = once(installed)
                    assert code == 0
                    assert [(p / 'recovery.json').read_bytes() for p in journals] == before
                    if case == 'pagination':
                        assert len(entries) == 302
                    ctl('start', UNIT)
                    results.append({'case': case, 'status': 'pass', 'reports': reports,
                                    'max_observation_delay_ms': max(v['observed_boottime_ms'] - request['deadline_boottime_ms'] for v in reports),
                                    'service_restarts': restarts})
                print(json.dumps(results[-1]), flush=True)
            finally:
                for guard in guards:
                    if guard.poll() is None:
                        guard.kill()
                    guard.wait(timeout=3)
                (group / 'cgroup.kill').write_text('1')
                for process in processes:
                    process.wait(timeout=3)
                group.rmdir()
                # Service is quiesced before removing only this fixture's data.
                ctl('stop', UNIT)
                for path in SPOOL.iterdir():
                    if path.is_dir():
                        shutil.rmtree(path)
                    else:
                        path.unlink()
                ctl('start', UNIT)
        ctl('stop', UNIT)
        SPOOL.chmod(0o755)
        code, entries = once(installed)
        assert code == 2 and not entries
        SPOOL.chmod(0o700)
        results.append({'case': 'nonprivate-spool', 'status': 'pass'})
        value = {'scope': 'Linux/systemd expiry reaper component; no storage fence', 'kernel': os.uname().release,
                 'systemd': ctl('--version').splitlines()[0], 'binary_sha256': hashlib.sha256(installed.read_bytes()).hexdigest(),
                 'unit_sha256': hashlib.sha256(unit.read_bytes()).hexdigest(),
                 'fixture_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'cases': results}
        args.output.write_text(json.dumps(value, indent=2) + '\n')
    finally:
        ctl('stop', UNIT)
        unit.unlink()
        ctl('daemon-reload')
        installed.unlink()
        if SPOOL.exists():
            shutil.rmtree(SPOOL)


if __name__ == '__main__':
    main()
