#!/usr/bin/env python3
"""Known historical concurrency bugs in popular crates, reproduced under ffs.

    ./known.py setup tokio-7922          # git worktrees of the pre-/post-fix commits + harness builds
    ./known.py run tokio-7922 --iterations 200
    ./known.py native tokio-7922 --runs 50

Each entry under known/<name>/ has a Cargo.toml.in (with @TOKIO@-style
placeholders for the checkout path) and src/main.rs: a short program that
performs the scenario of the bug report once and exits 101 if the library
panics, deadlocks (ffs reports that itself) or violates the property.  It
is built twice, against the commit before the fix and against the fix,
and explored under ffs from the start.  The fix build must show 0 failing
branches.
"""
import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FFS = HERE.parents[1] / 'target/release/ffs'
SRC = Path(os.path.expanduser('~/repos/crates-src'))
BUILD = SRC / 'known-build'

# name -> {crate dir under SRC, placeholder, fix commit (None: one 'head'
# variant at the checkout), post_features: cargo features for the fixed build}
KNOWN = {
    # runtime: fix race condition during the blocking pool shutdown (#7922):
    # a blocking thread woken by spawn_blocking while the pool shuts down
    # double-decrements num_idle_threads and panics.
    'tokio-7922': dict(crate='tokio', ph='@TOKIO@', fix='f21f2693'),
    # A write() deadlocks against a thread holding a read lock and taking
    # a second read() (#212, 2019).  Closed by the PR #533 merge, which
    # replaced read_recursive with RecursiveRwLock; plain read() is still
    # writer-preferring, so the scenario is expected to deadlock on both
    # sides: this entry checks the deadlock detector on a reported scenario.
    'parking_lot-212': dict(crate='parking_lot', ph='@PL@', fix='6be7fac'),
    # read_recursive() parked after a parked writer is never woken (#505);
    # PR #533's RecursiveRwLock (reader-preferring) is the fix.
    'parking_lot-505': dict(crate='parking_lot', ph='@PL@', fix='6be7fac', post_features='post'),
    # Stress harnesses with invariants, at the checkout's HEAD.
    'dashmap-stress': dict(crate='dashmap', ph='@DASHMAP@', fix=None),
    'channel-stress': dict(crate='crossbeam', ph='@CROSSBEAM@', fix=None),
    'tokio-sync-stress': dict(crate='tokio', ph='@TOKIO@', fix=None),
    # Notify permits under a watchpoint on the state word (prog arg `watch`).
    'tokio-notify-watch': dict(crate='tokio', ph='@TOKIO@', fix=None),
    # unlock_upgradable_fair did not hand off to the parked upgradable reader.
    'parking_lot-fair': dict(crate='parking_lot', ph='@PL@', fix='6be7fac'),
}


def sh(cmd, **kw):
    print('+', ' '.join(str(c) for c in cmd), file=sys.stderr)
    return subprocess.run([str(c) for c in cmd], **kw)


def variants(name):
    k = KNOWN[name]
    if k['fix'] is None:
        return {'head': (SRC / k['crate'], None)}
    return {'pre': (SRC / f"{name}-pre", f"{k['fix']}^"), 'post': (SRC / f"{name}-post", k['fix'])}


def exe(name, variant):
    binname = name.replace('-', '_')
    return BUILD / f'{name}-{variant}' / 'target/release' / binname


def setup(args):
    for name in args.names:
        k = KNOWN[name]
        repo = SRC / k['crate']
        if k['fix'] is not None:
            sh(['git', '-C', repo, 'fetch', '-q', '--depth', '500', 'origin'])
        for variant, (wt, rev) in variants(name).items():
            if rev is not None and not wt.exists():
                sh(['git', '-C', repo, 'worktree', 'add', '-q', wt, rev], check=True)
            d = BUILD / f'{name}-{variant}'
            d.mkdir(parents=True, exist_ok=True)
            src_link = d / 'src'
            if not src_link.exists():
                src_link.symlink_to(HERE / 'known' / name / 'src')
            (d / 'Cargo.toml').write_text((HERE / 'known' / name / 'Cargo.toml.in').read_text()
                                          .replace(k['ph'], str(wt)))
            feats = ['--features', k['post_features']] if variant == 'post' and k.get('post_features') else []
            r = sh(['cargo', 'build', '--release', '-q', *feats], cwd=d)
            if r.returncode:
                sys.exit(f'[known] build of {name} ({variant}) failed')
            print(f'[known] {name} {variant} ({rev or "HEAD"}): {exe(name, variant)}')


def run(args):
    for name in args.names:
        for variant in args.variants or variants(name):
            out = HERE / 'results' / 'known' / f'{name}-{variant}'
            r = sh([FFS, 'explore', '--iterations', args.iterations, '--keep-going', '--seed', args.seed,
                    '--output-dir', out, *args.ffs_arg, '--', exe(name, variant), *args.prog_arg],
                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
            out.mkdir(parents=True, exist_ok=True)
            (out / 'log.txt').write_text(r.stdout)
            m = re.search(r'explored (\d+) branches, (\d+) failing', r.stdout)
            panics = {}
            for msg in re.findall(r"panicked at [^\n]+:\n([^\n]+)", r.stdout):
                panics[msg[:100]] = panics.get(msg[:100], 0) + 1
            print(f'[known] {name} {variant}: {m.group(0) if m else r.stdout.strip().splitlines()[-1]}')
            for k, n in sorted(panics.items(), key=lambda kv: -kv[1])[:3]:
                print(f'          {n}x {k}')


def native(args):
    for name in args.names:
        for variant in args.variants or variants(name):
            fails = hangs = 0
            for _ in range(args.runs):
                try:
                    r = subprocess.run([exe(name, variant)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       timeout=args.native_timeout)
                    fails += r.returncode != 0
                except subprocess.TimeoutExpired:
                    hangs += 1
            print(f'[known] {name} {variant}: {fails}/{args.runs} native runs failed, {hangs} hung (> {args.native_timeout} s)')


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    for c in ['setup', 'run', 'native']:
        p = sub.add_parser(c)
        p.add_argument('names', nargs='*', default=list(KNOWN))
        p.add_argument('--variants', nargs='+', default=None)
        p.add_argument('--iterations', type=int, default=200)
        p.add_argument('--seed', type=int, default=0)
        p.add_argument('--runs', type=int, default=50)
        p.add_argument('--native-timeout', type=float, default=5)
        p.add_argument('--ffs-arg', action='append', default=[])
        p.add_argument('--prog-arg', action='append', default=[], help='argument for the harness')
    args = ap.parse_args()
    args.names = args.names or list(KNOWN)
    if args.variants is None:
        args.variants = None  # resolved per name below
    {'setup': setup, 'run': run, 'native': native}[args.cmd](args)


if __name__ == '__main__':
    main()
