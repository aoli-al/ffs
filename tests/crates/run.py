#!/usr/bin/env python3
"""Run popular Rust concurrency crates' own test suites under ffs.

    ./run.py build crossbeam tokio parking_lot dashmap   # cargo test --no-run (release + debug assertions)
    ./run.py list crossbeam                              # test binaries and the tests they hold
    ./run.py run crossbeam --bin channel --mode explore --iterations 20 -j 8
    ./run.py run tokio --bin sync_notify --test notify_waiters --mode explore --iterations 200
    ./run.py summary results/explore/crossbeam           # re-print a results.tsv

Each test is executed as its own ffs session: the crate's libtest binary
with `--exact <name> --test-threads=1 --nocapture`, explored from the
start (no checkpoint; these tests are milliseconds long).  A test counts
as failing when the binary panics (exit 101), crashes, deadlocks or hangs
in some schedule.  Results go to results/<mode>/<crate>/<bin>/<test>/.

The crates are plain git checkouts under --src (default
~/repos/crates-src/<crate>): nothing in them is patched.
"""
import argparse
import json
import zlib
import os
import re
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

HERE = Path(__file__).resolve().parent
FFS = HERE.parents[1] / 'target/release/ffs'
SRC = Path(os.path.expanduser('~/repos/crates-src'))
BUILD_ENV = {
    'CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS': 'true',
    'CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS': 'true',
    'CARGO_PROFILE_RELEASE_DEBUG': '1',
}
# cargo test arguments per crate (workspace members, features).
CRATES = {
    'crossbeam': ['--workspace', '--exclude', 'benchmarks'],
    'tokio': ['-p', 'tokio', '--features', 'full'],
    'parking_lot': ['--workspace'],
    'dashmap': [],
    'tokio-util': ['-p', 'tokio-util', '--features', 'full'],
    'moka': ['--features', 'sync,future'],
    'rayon': ['--workspace', '--exclude', 'rayon-demo'],
    'futures-rs': ['-p', 'futures-channel', '-p', 'futures-util', '-p', 'futures-executor', '--features', 'futures-executor/thread-pool'],
}


def sh(cmd, **kw):
    print('+', ' '.join(str(c) for c in cmd), file=sys.stderr)
    return subprocess.run([str(c) for c in cmd], **kw)


def build_json(crate):
    return HERE / 'build' / f'{crate}.json'


def cmd_build(args):
    (HERE / 'build').mkdir(exist_ok=True)
    for crate in args.crates:
        # tokio-util lives in the tokio checkout.
        src = SRC / ('tokio' if crate == 'tokio-util' else crate)
        extra = CRATES.get(crate, CRATES.get(crate.split('-')[0], ['--workspace']))
        r = sh(['cargo', 'test', '--release', '--no-run', *extra, '--message-format=json'],
               cwd=src, env={**os.environ, **BUILD_ENV}, stdout=subprocess.PIPE, text=True)
        if r.returncode:
            sys.exit(f'[run] build of {crate} failed')
        bins = []
        for line in r.stdout.splitlines():
            try:
                m = json.loads(line)
            except json.JSONDecodeError:
                continue
            if m.get('reason') == 'compiler-artifact' and m.get('executable') and m['profile'].get('test'):
                bins.append({'package': m['package_id'].split('#')[0].rsplit('/', 1)[-1],
                             'name': m['target']['name'], 'kind': m['target']['kind'][0],
                             'exe': m['executable'], 'manifest': m['manifest_path']})
        build_json(crate).write_text(json.dumps(bins, indent=1))
        print(f'[run] {crate}: {len(bins)} test binaries')


def binaries(crate):
    if not build_json(crate).exists():
        sys.exit(f'[run] run `./run.py build {crate}` first')
    return json.loads(build_json(crate).read_text())


def list_tests(exe):
    r = subprocess.run([exe, '--list', '--format', 'terse'], stdout=subprocess.PIPE, text=True)
    return [l[:-len(': test')] for l in r.stdout.splitlines() if l.endswith(': test')]


def select_bins(crate, want):
    bins = binaries(crate)
    if want:
        bins = [b for b in bins if b['name'] in want or f"{b['package']}/{b['name']}" in want]
    return bins


def cmd_list(args):
    for b in select_bins(args.crate, args.bin):
        tests = list_tests(b['exe'])
        print(f"{b['package']}/{b['name']} ({b['kind']}): {len(tests)} tests")
        if args.verbose:
            for t in tests:
                print('   ', t)


EXPLORED_RE = re.compile(r'explored (\d+) branches, (\d+) failing')
FAIL_RE = re.compile(r'^\[ffs\] branch (\d+): FAILURE (.*?)(?: after \d+ steps.*)?$', re.M)
PANIC_RE = re.compile(r"^thread '([^']*)' \(\d+\) panicked at ([^\n]+):\n([^\n]+)", re.M)


def failure_kinds(text):
    kinds = set()
    for _, what in FAIL_RE.findall(text):
        if what.startswith('deadlock'):
            kinds.add('DEADLOCK')
        elif what.startswith('hang'):
            kinds.add('HANG')
        elif what.startswith('killed'):
            kinds.add('CRASH')
        elif 'exited(101)' in what:
            kinds.add('PANIC')
        elif what.startswith('exited'):
            kinds.add('EXIT')
        elif 'step' in what or 'depth' in what:
            kinds.add('STEP_BOUND')
        else:
            kinds.add(re.sub(r'[^A-Z_]', '', what.upper().replace(' ', '_'))[:20] or 'OTHER')
    return kinds


def classify(mode, rc, text, timed_out):
    if timed_out:
        # The schedules that ran before the cap still count; failures
        # were printed as they happened (--keep-going).
        kinds = failure_kinds(text)
        return 'TIMEOUT+' + '+'.join(sorted(kinds)) if kinds else 'TIMEOUT'
    if '[ffs] error' in text:
        return 'FFS_ERROR'
    if mode == 'explore':
        m = EXPLORED_RE.search(text)
        if not m:
            return f'EXIT_{rc}'
        if int(m.group(2)) == 0:
            return 'PASS'
        kinds = failure_kinds(text)
        return '+'.join(sorted(kinds)) if kinds else 'FAIL'
    if 'DEADLOCK:' in text:
        return 'DEADLOCK'
    if 'HANG:' in text:
        return 'HANG'
    if 'killed by signal' in text:
        return 'CRASH'
    if rc == 0:
        return 'PASS'
    if rc == 101:
        return 'PANIC'
    return f'EXIT_{rc}'


def panic_summary(text):
    """Distinct panic messages (test-line or library) from a log."""
    out = {}
    for thread, where, msg in PANIC_RE.findall(text):
        key = f'{where.strip()}: {msg.strip()[:90]}'
        out[key] = out.get(key, 0) + 1
    return out


def run_test(args, crate, b, test, out_root):
    workdir = out_root / b['name'] / test.replace('::', '.')
    shutil.rmtree(workdir, ignore_errors=True)
    workdir.mkdir(parents=True)
    # Tests run in a private directory with a private TMPDIR: temp-file
    # names are deterministic under ffs's virtual randomness, so concurrent
    # sessions sharing /tmp would collide.  A copy of the package's
    # Cargo.toml is there for tests that read it (tokio's basic_fs).
    cwd = workdir / 'run'
    (cwd / 'tmp').mkdir(parents=True)
    shutil.copy(b['manifest'], cwd / 'Cargo.toml')
    prog = [b['exe'], '--exact', test, '--test-threads=1', '--nocapture']
    seed = ['--seed', str(zlib.crc32(test.encode()) % 1_000_000)]
    if args.mode == 'native':
        cmd = prog
    elif args.mode == 'record':
        cmd = [FFS, 'record', *seed, '-o', workdir / 'trace.json', *args.ffs_arg, '--', *prog]
    else:
        cmd = [FFS, 'explore', '--iterations', args.iterations, '--keep-going', *seed,
               '--output-dir', workdir, *(['--policy', args.policy] if args.policy else []),
               *args.ffs_arg, '--', *prog]
    t0 = time.time()
    timed_out = False
    try:
        p = subprocess.run([str(c) for c in cmd], cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           stdin=subprocess.DEVNULL, timeout=args.timeout,
                           env={**os.environ, 'RUST_BACKTRACE': '0', 'TMPDIR': str(cwd / 'tmp')})
        rc, text = p.returncode, p.stdout.decode(errors='replace')
    except subprocess.TimeoutExpired as e:
        rc, text, timed_out = -1, (e.stdout or b'').decode(errors='replace'), True
    secs = time.time() - t0
    (workdir / 'log.txt').write_text(text)
    status = classify(args.mode, rc, text, timed_out)
    if status == 'PASS':
        shutil.rmtree(workdir, ignore_errors=True)
    m = EXPLORED_RE.search(text)
    if m:
        explored = f'{m.group(2)}/{m.group(1)}'
    else:
        # Cut short: count the branches that reported.
        done = re.findall(r'^\[ffs\] branch (\d+): ', text, re.M)
        explored = f'{len(FAIL_RE.findall(text))}/{done[-1] if done else 0}+'
    panics = '; '.join(f'{n}x {k}' for k, n in sorted(panic_summary(text).items(), key=lambda kv: -kv[1])[:2])
    return b['name'], test, status, explored, f'{secs:.1f}', panics


def cmd_run(args):
    out_root = Path(args.out or HERE / 'results' / args.mode / args.crate).resolve()
    out_root.mkdir(parents=True, exist_ok=True)
    jobs = []
    for b in select_bins(args.crate, args.bin):
        tests = list_tests(b['exe'])
        if args.test:
            tests = [t for t in tests if any(t == w or t.endswith('::' + w) or re.search(w, t) for w in args.test)]
        jobs += [(b, t) for t in tests]
    tsv = out_root / 'results.tsv'
    if args.resume and tsv.exists():
        done = {tuple(l.split('\t')[:2]) for l in tsv.read_text().splitlines() if l.strip()}
        jobs = [(b, t) for b, t in jobs if (b['name'], t) not in done]
        print(f'[run] resume: {len(done)} tests already have results')
    print(f'[run] {args.crate}: {len(jobs)} tests, mode {args.mode}, {args.iterations} iterations, -j{args.j}')
    rows = []
    with ThreadPoolExecutor(max_workers=args.j) as ex, open(tsv, 'a') as f:
        futs = [ex.submit(run_test, args, args.crate, b, t, out_root) for b, t in jobs]
        for fut in as_completed(futs):
            row = fut.result()
            rows.append(row)
            print('\t'.join(row), file=f, flush=True)
            if row[2] != 'PASS' or args.verbose:
                print('[run]', '\t'.join(row), flush=True)
    print_summary(rows)


def print_summary(rows):
    counts = {}
    for r in rows:
        counts[r[2]] = counts.get(r[2], 0) + 1
    print('[run] summary:', ', '.join(f'{k} {v}' for k, v in sorted(counts.items(), key=lambda kv: -kv[1])))


def cmd_summary(args):
    rows = [l.rstrip('\n').split('\t') for l in open(Path(args.dir) / 'results.tsv') if l.strip()]
    latest = {}
    for r in rows:
        latest[(r[0], r[1])] = r
    rows = list(latest.values())
    for r in sorted(rows):
        if r[2] != 'PASS':
            print('\t'.join(r))
    print_summary(rows)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    b = sub.add_parser('build')
    b.add_argument('crates', nargs='+')
    l = sub.add_parser('list')
    l.add_argument('crate')
    l.add_argument('--bin', action='append')
    l.add_argument('-v', '--verbose', action='store_true')
    r = sub.add_parser('run')
    r.add_argument('crate')
    r.add_argument('--bin', action='append', help='test binary name (repeatable)')
    r.add_argument('--test', action='append', help='test name, suffix or regex (repeatable)')
    r.add_argument('--mode', choices=['native', 'record', 'explore'], default='explore')
    r.add_argument('--iterations', type=int, default=20)
    r.add_argument('--policy', default=None)
    r.add_argument('--timeout', type=float, default=600)
    r.add_argument('--ffs-arg', action='append', default=[])
    r.add_argument('--out', default=None)
    r.add_argument('-j', type=int, default=4)
    r.add_argument('--resume', action='store_true', help='skip tests that already have a row in results.tsv')
    r.add_argument('-v', '--verbose', action='store_true')
    s = sub.add_parser('summary')
    s.add_argument('dir')
    args = ap.parse_args()
    if args.cmd != 'build' and not FFS.exists():
        sys.exit(f'[run] build ffs first: {FFS} is missing')
    {'build': cmd_build, 'list': cmd_list, 'run': cmd_run, 'summary': cmd_summary}[args.cmd](args)


if __name__ == '__main__':
    main()
