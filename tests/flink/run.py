#!/usr/bin/env python3
"""Run Flink's own JUnit test classes (from the published *-tests.jar
artifacts) under ffs, one class or one method per JVM.

    mvn -q -B dependency:build-classpath -Dmdep.outputFile=classpath.txt
    ./run.py --module flink-connector-base --mode native          # baseline
    ./run.py --module flink-connector-base --mode record          # one schedule each
    ./run.py --module flink-connector-base --mode explore --iterations 20
    ./run.py --class ...FutureCompletingBlockingQueueTest --mode explore --root vtime:0.5

The JVM is started through JUnit's ConsoleLauncher with `--select-class`
(or `--select-method`), so no Flink code is modified and nothing calls into
ffs.  Failures are whatever the test reports (JUnit exit code 1), plus what
ffs finds: deadlocks, hangs, crashes.  Results go to a TSV (one line per
class: status, exit code, seconds, summary).
"""
import argparse
import os
import shutil
import re
import subprocess
import sys
import time
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
FFS = HERE.parents[1] / 'target/release/ffs'
LAUNCHER = 'org.junit.platform.console.ConsoleLauncher'
# The JVM lists /tmp/hsperfdata at startup (live state); fewer JIT threads
# and the serial collector make branches cheaper and startup more alike.
# -ea as Flink's surefire configuration does (KryoSerializer's concurrent-
# access check, for one, is only active with assertions).
# The module opens Flink's own JDK 17 configuration gives its test JVMs
# (without them Kryo's reflection fails and KryoSerializerConcurrencyTest
# hangs, natively too).
JDK17_OPENS = ['--add-exports=java.base/sun.net.util=ALL-UNNAMED',
               '--add-exports=java.rmi/sun.rmi.registry=ALL-UNNAMED',
               '--add-exports=java.management/sun.management=ALL-UNNAMED',
               '--add-exports=java.security.jgss/sun.security.krb5=ALL-UNNAMED'] + [
    f'--add-opens=java.base/{pkg}=ALL-UNNAMED' for pkg in
    ['java.lang', 'java.net', 'java.io', 'java.nio', 'sun.nio.ch', 'java.lang.reflect', 'java.text',
     'java.time', 'java.util', 'java.util.concurrent', 'java.util.concurrent.atomic',
     'java.util.concurrent.locks']]
JVM_FLAGS = ['-ea', '-XX:-UsePerfData', '-XX:+UseSerialGC', '-XX:TieredStopAtLevel=1',
             '-XX:+IgnoreUnrecognizedVMOptions', '-Xmx512m', '-Xss512k',
             '-Djava.awt.headless=true', *JDK17_OPENS]


def classpath():
    f = HERE / 'classpath.txt'
    if not f.exists():
        sys.exit('run `mvn -q -B dependency:build-classpath -Dmdep.outputFile=classpath.txt` in tests/flink first')
    return f.read_text().strip()


def test_classes(cp, module):
    jars = [j for j in cp.split(':') if re.search(rf'/{re.escape(module)}-[0-9][^/]*-tests\.jar$', j)]
    if not jars:
        sys.exit(f'no tests jar for {module} on the classpath')
    names = []
    with zipfile.ZipFile(jars[0]) as z:
        for n in z.namelist():
            if n.endswith('.class') and '$' not in n and re.search(r'(Test|ITCase)\.class$', n):
                names.append(n[:-6].replace('/', '.'))
    return sorted(names)


def junit_summary(text):
    """(tests found, failed, aborted) from ConsoleLauncher's summary."""
    def grab(what):
        m = re.search(rf'\[\s*(\d+) tests {what}\s*\]', text)
        return int(m.group(1)) if m else -1
    return grab('found'), grab('failed'), grab('aborted')


def classify(mode, rc, text, timed_out):
    found, failed, aborted = junit_summary(text)
    if timed_out:
        return 'TIMEOUT'
    if 'DEADLOCK:' in text or 'FAILURE deadlock' in text:
        return 'DEADLOCK'
    if 'HANG:' in text or 'FAILURE hang' in text:
        return 'HANG'
    if 'ASSERTION FAILED' in text:
        return 'ASSERT'
    if 'REPLAY DIVERGED' in text:
        return 'DIVERGED'
    if 'killed by signal' in text or 'FAILURE killed' in text:
        return 'CRASH'
    if '[ffs] error' in text:
        return 'FFS_ERROR'
    if found == 0:
        return 'NO_TESTS'
    if failed > 0 or aborted > 0 or 'FAILURE exited' in text:
        return 'TEST_FAIL'
    if rc == 0:
        return 'PASS'
    if 'step bound reached' in text:
        return 'STEP_BOUND'
    return f'EXIT_{rc}'


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--module', default='flink-connector-base', help='module whose tests jar to enumerate')
    ap.add_argument('--class', dest='klass', action='append', help='run only this class (repeatable); name or suffix')
    ap.add_argument('--method', help='run one method: Class#method')
    ap.add_argument('--mode', choices=['native', 'record', 'explore'], default='record')
    ap.add_argument('--iterations', type=int, default=20)
    ap.add_argument('--root', default='start', help='explore root (start, vtime:S, steps:N, ...)')
    ap.add_argument('--policy', default=None)
    ap.add_argument('--timeout', type=float, default=1800, help='seconds of wall time per class')
    ap.add_argument('--replay', action='store_true', help='record mode: also replay the trace (status DIVERGED if it does not follow)')
    ap.add_argument('--verify-data', action='store_true', help='with --replay: pass --verify-data to the replay (find the live input behind a divergence)')
    ap.add_argument('--out', default=None, help='results directory (default results/<mode>)')
    ap.add_argument('--ffs-arg', action='append', default=['--max-insn-steps', '0'],
                    help='extra argument for ffs (repeatable; default: no instruction stepping, '
                         'which costs 40%% of recording time on a JVM)')
    ap.add_argument('--skip', action='append', default=['TestCodeArchitectureTest'], help='class name fragments to skip')
    ap.add_argument('--keep-traces', action='store_true', help='record mode: keep the trace of a passing class too (they are large)')
    ap.add_argument('-j', type=int, default=1, help='parallel classes')
    args = ap.parse_args()

    cp = classpath()
    if args.method:
        targets = [args.method]
    elif args.klass:
        all_ = test_classes(cp, args.module)
        targets = []
        for k in args.klass:
            targets += [c for c in all_ if c == k or c.endswith('.' + k) or c.endswith(k)] or [k]
    else:
        targets = [c for c in test_classes(cp, args.module) if not any(s in c for s in args.skip)]
    out = Path(args.out or HERE / 'results' / args.mode)
    out.mkdir(parents=True, exist_ok=True)
    tsv = out / 'results.tsv'

    def run_one(target):
        select = ['--select-method', target] if '#' in target else ['--select-class', target]
        java = ['java', *JVM_FLAGS, '-cp', cp, LAUNCHER, 'execute', '--disable-banner',
                '--details=summary', '--disable-ansi-colors', *select]
        workdir = out / target.replace('#', '.')
        workdir.mkdir(exist_ok=True)
        # The JVM runs in a fresh directory that holds nothing but a private
        # temp dir, recreated identically before a replay: a directory
        # listing must see the same entries, and under virtual randomness
        # every run draws the same "random" temp-file names (Flink's RPC
        # loader extracts a jar to /tmp/flink-rpc-akka<UUID>.jar), so runs
        # would collide in a shared /tmp.  ffs's outputs stay outside it.
        cwd = workdir / 'run'
        tmp = cwd / 'tmp'

        def fresh_cwd():
            shutil.rmtree(cwd, ignore_errors=True)
            tmp.mkdir(parents=True)
        fresh_cwd()
        java = java[:1] + [f'-Djava.io.tmpdir={tmp}'] + java[1:]
        # A different seed per class, so the module's classes do not all
        # see the same schedule and the same random draws.
        seed = ['--seed', str(abs(hash(target)) % 1_000_000)]
        if args.mode == 'native':
            cmd = java
        elif args.mode == 'record':
            cmd = [str(FFS), 'record', '--timeline', *seed, '-o', str(workdir / 'trace.json'), *args.ffs_arg, '--', *java]
        else:
            cmd = [str(FFS), 'explore', '--root', args.root, '--iterations', str(args.iterations), *seed,
                   '--output-dir', str(workdir), *(['--policy', args.policy] if args.policy else []),
                   *args.ffs_arg, '--', *java]
        t0 = time.time()
        timed_out = False
        try:
            p = subprocess.run(cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               stdin=subprocess.DEVNULL, timeout=args.timeout)
            rc, text = p.returncode, p.stdout.decode(errors='replace')
        except subprocess.TimeoutExpired as e:
            rc, text, timed_out = -1, (e.stdout or b'').decode(errors='replace'), True
        secs = time.time() - t0
        (workdir / 'log.txt').write_text(text)
        status = classify(args.mode, rc, text, timed_out)
        if args.mode == 'record' and status == 'PASS' and not args.keep_traces and not args.replay:
            (workdir / 'trace.json').unlink(missing_ok=True)
        if args.mode == 'record' and args.replay and status in ('PASS', 'TEST_FAIL', 'DEADLOCK', 'HANG', 'ASSERT'):
            # Same kinds of standard descriptors as the recording (all
            # captured), same working directory, and the same empty temp
            # directory: files the recording left there would show up in
            # directory listings.
            fresh_cwd()
            try:
                p = subprocess.run([str(FFS), 'replay', *(['--verify-data'] if args.verify_data else []), str(workdir / 'trace.json')], cwd=cwd,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL,
                                   timeout=args.timeout)
                rtext = p.stdout.decode(errors='replace')
            except subprocess.TimeoutExpired as e:
                rtext = (e.stdout or b'').decode(errors='replace') + '\n[ffs] replay TIMEOUT'
            (workdir / 'replay.txt').write_text(rtext)
            if 'replayed all' not in rtext:
                status = 'DIVERGED' if 'DIVERGED' in rtext else 'REPLAY_' + ('TIMEOUT' if 'TIMEOUT' in rtext else 'ERROR')
                text += rtext
            elif status == 'PASS' and not args.keep_traces:
                (workdir / 'trace.json').unlink(missing_ok=True)
            secs = time.time() - t0
        found, failed, aborted = junit_summary(text)
        ffs_lines = [l for l in text.splitlines() if l.startswith('[ffs]') and
                     re.search(r'FAILURE|DEADLOCK|HANG|ASSERTION|DIVERGED|killed|error|explored|virtual time|recorded', l)]
        summary = ffs_lines[-1] if ffs_lines else ''
        return target, status, rc, secs, found, failed, summary

    results = []
    from concurrent.futures import ThreadPoolExecutor, as_completed
    with ThreadPoolExecutor(max_workers=args.j) as ex:
        for fut in as_completed([ex.submit(run_one, t) for t in targets]):
            r = fut.result()
            results.append(r)
            target, status, rc, secs, found, failed, summary = r
            print(f'{status:10} {secs:7.1f}s  tests={found:<3} failed={failed:<3} {target}  {summary}', flush=True)
    results.sort(key=lambda r: r[0])
    with open(tsv, 'w') as f:
        f.write('class\tstatus\texit\tseconds\ttests\tfailed\tsummary\n')
        for target, status, rc, secs, found, failed, summary in results:
            f.write(f'{target}\t{status}\t{rc}\t{secs:.1f}\t{found}\t{failed}\t{summary}\n')
    from collections import Counter
    print('\n' + ', '.join(f'{k}={v}' for k, v in sorted(Counter(r[1] for r in results).items())), '->', tsv)
    return 0 if all(r[1] == 'PASS' for r in results) else 1


if __name__ == '__main__':
    sys.exit(main())
