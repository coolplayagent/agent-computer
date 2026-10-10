#!/usr/bin/env python3
"""Root-only journal IO fault probe in a disposable VM; no writer-release proof."""
import argparse
import hashlib
import json
import os
import pathlib
import select
import signal
import subprocess
import tempfile
import time
import uuid


def now():
    return time.clock_gettime_ns(time.CLOCK_BOOTTIME) // 1_000_000


def frame(pipe):
    assert select.select([pipe], [], [], 8)[0], 'missing receipt'
    return json.loads(pipe.readline())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--watchdog', required=True, type=pathlib.Path)
    parser.add_argument('--output', required=True, type=pathlib.Path)
    args = parser.parse_args()
    assert os.geteuid() == 0
    binary = args.watchdog.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix='ac-journal-', dir='/root') as temporary:
        work = pathlib.Path(temporary)
        for name in ['normal', 'closed-output', 'report-write-error', 'full-output', 'duplicate-journal', 'nonprivate-journal']:
            group = pathlib.Path('/sys/fs/cgroup') / ('ac-journal-' + uuid.uuid4().hex)
            group.mkdir()
            child = subprocess.Popen(['/bin/sleep', '60'])
            guard = None
            read_fd = None
            try:
                (group / 'cgroup.procs').write_text(str(child.pid))
                child.send_signal(signal.SIGSTOP)
                journal = work / name
                journal.mkdir(mode=0o700)
                value = {'version': 1, 'execution_id': name,
                         'boot_id': pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
                         'cgroup_path': group.name, 'cgroup_inode': group.stat().st_ino,
                         'deadline_boottime_ms': now() + (25000 if name == 'full-output' else 1200)}
                request = work / (name + '.json')
                request.write_text(json.dumps(value))
                if name == 'report-write-error':
                    (journal / 'report.pending').write_bytes(b'partial')
                elif name == 'duplicate-journal':
                    (journal / 'intent.json').write_bytes(b'historical')
                elif name == 'nonprivate-journal':
                    journal.chmod(0o777)
                output = subprocess.PIPE
                if name == 'full-output':
                    read_fd, write_fd = os.pipe2(os.O_NONBLOCK)
                    try:
                        while True:
                            os.write(write_fd, b'x' * 4096)
                    except BlockingIOError:
                        pass
                    output = write_fd
                guard = subprocess.Popen([str(binary), '--request', str(request), '--journal', str(journal)],
                                         stdout=output, stderr=subprocess.PIPE, bufsize=0, start_new_session=True)
                if name == 'full-output':
                    os.close(write_fd)
                    assert guard.wait(timeout=5) == 2
                    saved = json.loads((journal / 'report.json').read_bytes())
                    assert saved['trigger'] == 'ReceiptUnavailable' and saved['observation'] == 'EmptyObserved'
                    assert saved['kill_boottime_ms'] < value['deadline_boottime_ms']
                    assert 'populated 0\n' in (group / 'cgroup.events').read_text()
                    record = {'case': name, 'status': 'pass', 'report': saved}
                elif name in ['duplicate-journal', 'nonprivate-journal']:
                    stdout, stderr = guard.communicate(timeout=3)
                    assert guard.returncode == 2 and not stdout, (stdout, stderr)
                    assert 'populated 1\n' in (group / 'cgroup.events').read_text()
                    if name == 'duplicate-journal':
                        assert (journal / 'intent.json').read_bytes() == b'historical'
                    record = {'case': name, 'status': 'pass', 'armed': False, 'target_untouched': True}
                else:
                    armed = frame(guard.stdout)
                    assert armed['request'] == value
                    intent_bytes = (journal / 'intent.json').read_bytes()
                    intent = json.loads(intent_bytes)
                    assert intent['request'] == value and intent['watchdog_pid'] == guard.pid
                    assert armed['journal']['intent_digest'] == 'sha256:' + hashlib.sha256(intent_bytes).hexdigest()
                    assert armed['journal']['inode'] == journal.stat().st_ino
                    if name == 'closed-output':
                        guard.stdout.close()
                    elif name == 'normal':
                        report = frame(guard.stdout)
                        assert report['journal'] == armed['journal']
                    assert guard.wait(timeout=8) == (0 if name == 'normal' else 2)
                    assert 'populated 0\n' in (group / 'cgroup.events').read_text()
                    if name == 'report-write-error':
                        assert not (journal / 'report.json').exists()
                        assert (journal / 'report.pending').read_bytes() == b'partial'
                        record = {'case': name, 'status': 'pass', 'target_empty': True, 'report_missing': True}
                    else:
                        saved = json.loads((journal / 'report.json').read_bytes())
                        assert saved['observation'] == 'EmptyObserved' and saved['error'] is None
                        assert saved['request'] == value and saved['journal'] == armed['journal']
                        assert saved['kill_boottime_ms'] >= value['deadline_boottime_ms']
                        assert (journal / 'report.json').stat().st_mode & 0o777 == 0o600
                        record = {'case': name, 'status': 'pass', 'report': saved}
                results.append(record)
                print(json.dumps(record), flush=True)
            finally:
                if read_fd is not None:
                    os.close(read_fd)
                (group / 'cgroup.kill').write_text('1')
                child.wait(timeout=5)
                if guard is not None:
                    guard.wait(timeout=8)
                group.rmdir()
    output = {'scope': 'Linux journal IO component, not Computer acceptance', 'kernel': os.uname().release,
              'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'fixture_sha256': hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest(), 'cases': results}
    args.output.write_text(json.dumps(output, indent=2) + '\n')


if __name__ == '__main__':
    main()
