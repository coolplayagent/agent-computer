#!/usr/bin/env python3
"""Root-only disposable VM probe: real renewal pipes, timers, journals and cgroups.

The frozen-journal case mounts a fresh private loop filesystem, never the root
filesystem. These are node observations, not reconstructed writer-release proof.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import time
import uuid


def now():
    return time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1_000_000


def live(group):
    return 'populated 1\n' in (group / 'cgroup.events').read_text()


def until(predicate, seconds=6):
    limit = time.monotonic() + seconds
    while time.monotonic() < limit:
        if predicate():
            return
        time.sleep(.01)
    raise AssertionError('observation timed out')


def frame(pipe, timeout=2):
    assert select.select([pipe], [], [], timeout)[0], 'missing frame'
    value = pipe.readline()
    assert value, 'unexpected EOF'
    return json.loads(value)


def send(guard, command):
    guard.stdin.write(json.dumps(command, separators=(',', ':')).encode() + b'\n')
    guard.stdin.flush()


def reap(binary, spool, observations):
    limit = time.monotonic() + 3
    while True:
        probe = subprocess.run([str(binary), '--reap-once', '--spool', str(spool)], capture_output=True, text=True, timeout=4)
        value = json.loads(probe.stdout)
        observations.append(value)
        if probe.returncode == 0:
            return
        # cgroup.kill can return before the kernel has reaped the last member.
        # The reaper deliberately retries that observation on the next pass.
        assert probe.returncode == 2 and all(
            entry['outcome']['state'] != 'unavailable' or entry['outcome'].get('error') == 'DrainTimeout'
            for entry in value['entries']), (probe.stdout, probe.stderr)
        assert time.monotonic() < limit, value
        time.sleep(.02)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--watchdog', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    assert os.geteuid() == 0 and Path('/run/systemd/system').is_dir(), 'disposable root VM required'
    binary = args.watchdog.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix='ac-renewal-', dir='/root') as tmp:
        root = Path(tmp)
        for case in ['renewed-deadline', 'closed-control', 'replay', 'lost-output',
                     'partial-dual', 'frozen-journal', 'reaper-renewed', 'reaper-corrupt-head']:
            work = root / case
            work.mkdir(mode=0o700)
            group = Path('/sys/fs/cgroup') / ('ac-renewal-' + uuid.uuid4().hex)
            group.mkdir()
            workload = subprocess.Popen(['/bin/sleep', '60'])
            guards, journals, receipts, reaper_passes = [], [], [], []
            mounted = frozen = False
            mount = work / 'mount'
            try:
                (group / 'cgroup.procs').write_text(str(workload.pid))
                workload.send_signal(signal.SIGSTOP)
                spool = work / 'spool'
                if case == 'frozen-journal':
                    disk = work / 'journal.ext4'
                    with disk.open('wb') as file:
                        file.truncate(32 * 1024 * 1024)
                    subprocess.run(['mkfs.ext4', '-q', '-F', str(disk)], check=True, timeout=10)
                    mount.mkdir(mode=0o700)
                    subprocess.run(['mount', '-o', 'loop', str(disk), str(mount)], check=True, timeout=10)
                    mounted = True
                    mount.chmod(0o700)
                    spool = mount / 'spool'
                spool.mkdir(mode=0o700)
                request = {'version': 2, 'execution_id': case,
                           'boot_id': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                           'cgroup_path': group.name, 'cgroup_inode': group.stat().st_ino,
                           'deadline_boottime_ms': now() + 1600,
                           'renewal': {'hard_deadline_boottime_ms': now() + 7000,
                                       'authority_digest': 'sha256:' + 'a' * 64}}
                # Rust serializes the typed request in declaration order.
                digest = 'sha256:' + hashlib.sha256(b'agent-computer/node-renewal-request-v1\0' +
                    json.dumps(request, separators=(',', ':')).encode()).hexdigest()
                source = work / 'request.json'
                source.write_text(json.dumps(request))
                count = 2 if case.startswith('reaper-') or case == 'partial-dual' else 1
                for index in range(count):
                    journal = spool / ('journal-' + str(index))
                    journal.mkdir(mode=0o700)
                    journals.append(journal)
                    guard = subprocess.Popen([str(binary), '--request', str(source), '--journal', str(journal)],
                                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                             bufsize=0, start_new_session=True)
                    guards.append(guard)
                    armed = frame(guard.stdout)
                    assert armed['request'] == request
                    receipts.append(armed)
                command = {'version': 1, 'request_digest': digest, 'sequence': 1,
                           'grant_digest': 'sha256:' + 'b' * 64, 'deadline_boottime_ms': now() + 3200}
                assert command['deadline_boottime_ms'] > request['deadline_boottime_ms']
                if case == 'frozen-journal':
                    subprocess.run(['fsfreeze', '--freeze', str(mount)], check=True, timeout=5)
                    frozen = True
                if case == 'partial-dual':
                    guards[1].send_signal(signal.SIGSTOP)
                    until(lambda: '\nState:\tT' in Path(f'/proc/{guards[1].pid}/status').read_text())
                if case == 'lost-output':
                    guards[0].stdout.close()
                for guard in guards:
                    send(guard, command)
                acknowledgments = []
                if case not in ['frozen-journal', 'lost-output']:
                    for index, guard in enumerate(guards[:1] if case == 'partial-dual' else guards):
                        ack = frame(guard.stdout)
                        assert ack['event'] == 'renewed' and ack['command'] == command
                        assert ack['journal'] == receipts[index]['journal']
                        assert ack['previous_deadline_boottime_ms'] == request['deadline_boottime_ms']
                        assert json.loads((journals[index] / 'renewal.json').read_bytes()) == ack
                        assert json.loads((journals[index] / 'renewal-00000001.json').read_bytes()) == ack
                        acknowledgments.append(ack)
                if case in ['closed-control', 'partial-dual']:
                    # This is how the live adapter handles a missing second ack.
                    for guard in guards:
                        guard.stdin.close()
                    until(lambda: not live(group), 1)
                    assert now() < command['deadline_boottime_ms']
                elif case == 'replay':
                    send(guards[0], command)
                    until(lambda: not live(group), 1)
                    assert now() < command['deadline_boottime_ms']
                elif case.startswith('reaper-'):
                    for guard in guards:
                        guard.kill()
                        guard.wait(timeout=2)
                    if case == 'reaper-corrupt-head':
                        for journal in journals:
                            (journal / 'renewal.json').write_bytes(b'{')
                    until(lambda: now() > request['deadline_boottime_ms'] + 50)
                    reap(binary, spool, reaper_passes)
                    if case == 'reaper-renewed':
                        assert live(group), 'old deadline must not replace valid accepted renewal'
                        until(lambda: now() > command['deadline_boottime_ms'] + 50)
                        # A new process reopens the same journals; it never renews.
                        reap(binary, spool, reaper_passes)
                    assert not live(group)
                elif case == 'frozen-journal':
                    assert not select.select([guards[0].stdout], [], [], .1)[0], 'ack before journal persistence'
                    until(lambda: not live(group), 5)
                    assert command['deadline_boottime_ms'] <= now() <= command['deadline_boottime_ms'] + 1000
                    # The process may be blocked publishing its final report, but
                    # the independent timer has already killed the stopped tree.
                    killed_while_frozen = now()
                    subprocess.run(['fsfreeze', '--unfreeze', str(mount)], check=True, timeout=5)
                    frozen = False
                    assert guards[0].wait(timeout=5) in [0, 2]
                elif case == 'lost-output':
                    until(lambda: not live(group), 2)
                    assert now() < command['deadline_boottime_ms']
                else:
                    until(lambda: now() > request['deadline_boottime_ms'] + 50)
                    assert live(group), 'renewed timer did not survive the old deadline'
                    report = frame(guards[0].stdout, 5)
                    assert report['renewal'] == acknowledgments[0]
                    assert report['trigger'] == 'Deadline' and report['observation'] == 'EmptyObserved'
                    assert command['deadline_boottime_ms'] <= report['kill_boottime_ms'] <= command['deadline_boottime_ms'] + 1000
                    assert not live(group)
                record = {'case': case, 'status': 'pass', 'request': request, 'command': command,
                          'acknowledgments': acknowledgments, 'empty_observed_boottime_ms': now(), 'reaper_passes': reaper_passes}
                if case == 'frozen-journal':
                    record['empty_while_filesystem_frozen_boottime_ms'] = killed_while_frozen
                results.append(record)
                print(json.dumps(record), flush=True)
            finally:
                if frozen:
                    subprocess.run(['fsfreeze', '--unfreeze', str(mount)], check=True, timeout=5)
                for guard in guards:
                    if guard.poll() is None:
                        guard.kill()
                    guard.wait(timeout=5)
                (group / 'cgroup.kill').write_text('1')
                workload.wait(timeout=5)
                group.rmdir()
                if mounted:
                    subprocess.run(['umount', str(mount)], check=True, timeout=10)
    args.output.write_text(json.dumps({'scope': 'node component; not Computer completion authority',
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'fixture_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), 'cases': results}, indent=2) + '\n')


if __name__ == '__main__':
    main()
