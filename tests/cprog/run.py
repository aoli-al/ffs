#!/usr/bin/env python3
"""Run standalone C/C++ multithreaded test programs under ffs explore.

    ./run.py programs.tsv --iterations 100 -j 3 [--only NAME ...]
    ./run.py summary results/<set>

programs.tsv columns (tab-separated, `#` comments):
    name    cwd    ffs options (or -)    command and arguments

Each program is one ffs session explored from its start (or from its
first ffs_checkpoint); a program fails when some schedule exits non-zero,
crashes, deadlocks, hangs or hits the step bound.  Logs and failing
schedules go to <out>/<name>/.
"""
import argparse
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
import zlib
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

HERE = Path(__file__).resolve().parent
FFS = HERE.parents[1] / 'target/release/ffs'
sys.path.insert(0, str(HERE.parent / 'crates'))
from run import EXPLORED_RE, FAIL_RE, classify, failure_kinds, print_summary  # noqa: E402


def load(path):
    progs = []
    for line in open(path):
        line = line.rstrip('\n')
        if not line.strip() or line.startswith('#'):
            continue
        name, cwd, opts, cmd = line.split('\t', 3)
        progs.append((name, os.path.expanduser(cwd), [] if opts.strip() == '-' else shlex.split(opts), shlex.split(cmd)))
    return progs


def run_one(args, prog, out_root):
    name, cwd, opts, cmd = prog
    workdir = out_root / name
    shutil.rmtree(workdir, ignore_errors=True)
    (workdir / 'tmp').mkdir(parents=True)
    seed = ['--seed', str(zlib.crc32(name.encode()) % 1_000_000)]
    iterations = args.iterations
    if '--iterations' in opts:  # per-program override in programs.tsv
        i = opts.index('--iterations')
        iterations = int(opts[i + 1])
        opts = opts[:i] + opts[i + 2:]
    ffs_cmd = [str(FFS), 'explore', '--iterations', str(iterations), '--keep-going', *seed,
               '--output-dir', str(workdir), *opts, *args.ffs_arg, '--', *cmd]
    t0 = time.time()
    timed_out = False
    try:
        p = subprocess.run(ffs_cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           stdin=subprocess.DEVNULL, timeout=args.timeout,
                           env={**os.environ, 'TMPDIR': str(workdir / 'tmp')})
        rc, text = p.returncode, p.stdout.decode(errors='replace')
    except subprocess.TimeoutExpired as e:
        rc, text, timed_out = -1, (e.stdout or b'').decode(errors='replace'), True
    secs = time.time() - t0
    (workdir / 'log.txt').write_text(text)
    status = classify('explore', rc, text, timed_out)
    m = EXPLORED_RE.search(text)
    if m:
        explored = f'{m.group(2)}/{m.group(1)}'
    else:
        done = re.findall(r'^\[ffs\] branch (\d+): ', text, re.M)
        explored = f'{len(FAIL_RE.findall(text))}/{done[-1] if done else 0}+'
    last = [l for l in text.splitlines() if l.startswith('[ffs]')]
    return name, ' '.join(cmd)[:60], status, explored, f'{secs:.1f}', (last[-1] if last else '')[:120]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('programs', help='programs.tsv, or a results directory with `summary`')
    ap.add_argument('--only', action='append', help='program name or regex (repeatable)')
    ap.add_argument('--iterations', type=int, default=100)
    ap.add_argument('--timeout', type=float, default=1800)
    ap.add_argument('--ffs-arg', action='append', default=[])
    ap.add_argument('--out', default=None)
    ap.add_argument('-j', type=int, default=3)
    ap.add_argument('--resume', action='store_true', help='skip programs that already have a row in results.tsv')
    args = ap.parse_args()
    if args.programs == 'summary':
        sys.exit('usage: ./run.py summary <dir>')
    if Path(args.programs).is_dir():
        rows = [l.rstrip('\n').split('\t') for l in open(Path(args.programs) / 'results.tsv') if l.strip()]
        latest = {r[0]: r for r in rows}
        for r in sorted(latest.values()):
            if r[2] != 'PASS':
                print('\t'.join(r))
        print_summary(list(latest.values()))
        return
    progs = load(args.programs)
    if args.only:
        progs = [p for p in progs if any(p[0] == o or re.search(o, p[0]) for o in args.only)]
    out_root = Path(args.out or HERE / 'results' / Path(args.programs).stem).resolve()
    out_root.mkdir(parents=True, exist_ok=True)
    tsv = out_root / 'results.tsv'
    if args.resume and tsv.exists():
        done = {l.split('\t')[0] for l in tsv.read_text().splitlines() if l.strip()}
        progs = [p for p in progs if p[0] not in done]
        print(f'[run] resume: {len(done)} programs already have results')
    print(f'[run] {len(progs)} programs, {args.iterations} schedules each, -j{args.j}')
    rows = []
    with ThreadPoolExecutor(max_workers=args.j) as ex, open(out_root / 'results.tsv', 'a') as f:
        for fut in as_completed([ex.submit(run_one, args, p, out_root) for p in progs]):
            row = fut.result()
            rows.append(row)
            print('\t'.join(row), file=f, flush=True)
            print('[run]', '\t'.join(row[:5]), flush=True)
    print_summary(rows)


if __name__ == '__main__':
    main()
