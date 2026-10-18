#!/usr/bin/env python3
"""Test liquid-cache (https://github.com/datafusion-contrib/liquid-cache)
under ffs.

    ./run.py setup                 # patch + build the harness against a checkout
    ./run.py leak                  # sequential disk-budget leak (no ffs needed)
    ./run.py mixed --seeds 1 2 3   # random workload, 3 threads: crashes + drift
    ./run.py two-readers           # minimal double-hydration race (watchpoint)
    ./run.py flush-evict           # flush vs eviction of the same entry
    ./run.py replay OUT/.../fail-1.json
    ./run.py all

The harness (harness/src/main.rs) drives `LiquidCache` from several threads
and checks the cache's own invariants; `ffs explore` chooses the
interleavings.  `setup` needs a liquid-cache checkout (`--lc`, default
~/repos/liquid-cache-src/liquid-cache) and runs cargo inside that
checkout's `nix develop` shell.  Its `t4` store is switched to the thread
backend (`default-features = false`): the io_uring one cannot run under ffs
(io_uring_setup is ENOSYS there).

Every exploration gets a fresh directory under --out with the t4 store in
it (`--store-dir`): tempdir names are deterministic under ffs, so a reused
TMPDIR changes the recorded mkdir results and breaks replay.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FFS = HERE.parents[1] / 'target/release/ffs'
HARNESS = HERE / 'harness'
BIN = HARNESS / 'target/release/lc_harness'
T4_LINE = 't4 = { version = "0.1.9", default-features = false }'


def sh(cmd, **kw):
    print('+', ' '.join(str(c) for c in cmd), file=sys.stderr)
    return subprocess.run([str(c) for c in cmd], **kw)


def setup(args):
    lc = Path(args.lc).resolve()
    cargo_toml = lc / 'Cargo.toml'
    text = cargo_toml.read_text()
    if T4_LINE not in text:
        new = re.sub(r'^t4 = "0\.1\.9"$', T4_LINE, text, flags=re.M)
        assert new != text, 'could not find the t4 dependency line in ' + str(cargo_toml)
        cargo_toml.write_text(new)
        print(f'[run] patched {cargo_toml}: t4 without io_uring')
    (HARNESS / 'Cargo.toml').write_text(
        (HARNESS / 'Cargo.toml.in').read_text().replace('@LC@', str(lc)))
    if not (HARNESS / 'Cargo.lock').exists():
        shutil.copy(lc / 'Cargo.lock', HARNESS / 'Cargo.lock')
    r = sh(['nix', 'develop', '--command', 'cargo', 'build', '--release',
            '--manifest-path', HARNESS / 'Cargo.toml'], cwd=lc)
    if r.returncode:
        sys.exit('[run] build failed')
    if not FFS.exists():
        sys.exit(f'[run] build ffs first: {FFS} is missing')


def fresh(out, name):
    d = out / name
    if d.exists():
        shutil.rmtree(d)
    d.mkdir(parents=True)
    return d


def harness_args(store, extra):
    return [BIN, '--store-dir', store / 'store', *extra]


FAIL_RE = re.compile(r'^\[ffs\] branch (\d+): FAILURE (.*?)(?: after \d+ steps.*)?$', re.M)
PANIC_RE = re.compile(r"^thread '([^']+)' \(\d+\) panicked at ([^\n]+):\n([^\n]+)", re.M)


def explore(d, extra, ffs_opts, iterations, seed):
    store = fresh(d, f'x{seed:03d}')
    cmd = [FFS, 'explore', '--iterations', iterations, '--seed', seed, '--keep-going',
           '--output-dir', d / f'fail-{seed}', *ffs_opts, '--', *harness_args(store, extra)]
    r = sh(cmd, capture_output=True, text=True)
    (d / f'explore-{seed}.log').write_text(r.stderr)
    failures = FAIL_RE.findall(r.stderr)
    panics = {}
    for _, where, msg in PANIC_RE.findall(r.stderr):
        if 'main.rs' in where:
            continue
        key = f'{where.split("/src/core/src/")[-1]}: {msg.strip()[:70]}'
        panics[key] = panics.get(key, 0) + 1
    explored = re.search(r'explored (\d+) branches, (\d+) failing', r.stderr)
    print(f'[run] {d.name} seed {seed}: {explored.group(0) if explored else "?"}')
    kinds = {}
    for _, what in failures:
        k = re.sub(r'\d+', 'N', what)
        kinds[k] = kinds.get(k, 0) + 1
    for k, n in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print(f'      {n:3d} x {k}')
    for k, n in sorted(panics.items(), key=lambda kv: -kv[1]):
        # Each failure is executed twice (the verification re-execution).
        print(f'      {n:3d} panics: {k}')
    return failures, panics


MIXED = ['-v']  # the default random workload; -v logs every operation
TWO_READERS = ['--prefill', '1', '--keys', '1', '--identities', '1',
               '--script', 'get:0:0', '--script', 'get:0:0']
FLUSH_EVICT = ['--prefill', '3', '--no-prefill-flush', '--keys', '4', '--identities', '1',
               '--mem-entries', '3.5', '--script', 'flush', '--script', 'insert:3:0']


def cmd_mixed(args):
    out = Path(args.out)
    for seed in args.seeds:
        d = out / 'mixed'
        d.mkdir(parents=True, exist_ok=True)
        explore(d, MIXED, ['--max-depth', '20000'], args.iterations, seed)


def cmd_watch(args, name, extra):
    """A minimal two-thread scenario: a hardware watchpoint on the memory
    budget counter plus `--watch-hold` (the thread that touched it is held
    inside its read-modify-write window) makes the index/budget races
    reachable; without them 1600 schedules found nothing."""
    out = Path(args.out)
    d = out / name
    d.mkdir(parents=True, exist_ok=True)
    # The harness registers the counter's heap address itself (ffs_watch):
    # the address is deterministic within a run but differs between
    # `ffs record` and `ffs explore` (224 bytes here), so it cannot be
    # learned from a recording and passed as --watch.
    for seed in args.seeds:
        explore(d, [*extra, '--watch-budget'], ['--watch-hold', str(args.watch_hold),
                                                '--policy', args.policy], args.iterations, seed)


def cmd_leak(args):
    """insert -> flush -> get (hydrate) -> flush: every cycle charges the
    disk budget for a blob that overwrites the previous one."""
    out = Path(args.out)
    store = fresh(out / 'leak', 'store-parent')
    r = sh([BIN, '--store-dir', store / 'store', '--prefill', '1', '--keys', '1',
            '--identities', '1', '--check-disk', '--script',
            'get:0:0,flush,get:0:0,flush,get:0:0,flush'], capture_output=True, text=True)
    for line in r.stderr.splitlines():
        if 'final:' in line or 'ASSERTION' in line or 'prefilled' in line:
            print('[run]', line)


def cmd_replay(args):
    trace = json.load(open(args.trace))
    argv = trace['argv']
    if '--store-dir' in argv:
        store = Path(argv[argv.index('--store-dir') + 1])
        if store.exists():
            shutil.rmtree(store)
    r = sh([FFS, 'replay', *args.replay_opts, args.trace])
    sys.exit(r.returncode)


def main():
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument('--lc', default=os.path.expanduser('~/repos/liquid-cache-src/liquid-cache'))
    common.add_argument('--out', default=str(HERE / 'out'))
    common.add_argument('--iterations', type=int, default=200)
    common.add_argument('--seeds', type=int, nargs='+', default=[1])
    common.add_argument('--watch-hold', type=float, default=0.7)
    common.add_argument('--policy', default='mix')
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    for name in ['setup', 'leak', 'mixed', 'two-readers', 'flush-evict', 'all']:
        sub.add_parser(name, parents=[common])
    rp = sub.add_parser('replay', parents=[common])
    rp.add_argument('trace')
    rp.add_argument('replay_opts', nargs='*')
    args = ap.parse_args()
    if args.cmd == 'setup':
        setup(args)
    elif args.cmd == 'leak':
        cmd_leak(args)
    elif args.cmd == 'mixed':
        cmd_mixed(args)
    elif args.cmd == 'two-readers':
        cmd_watch(args, 'two-readers', TWO_READERS)
    elif args.cmd == 'flush-evict':
        cmd_watch(args, 'flush-evict', FLUSH_EVICT)
    elif args.cmd == 'replay':
        cmd_replay(args)
    elif args.cmd == 'all':
        setup(args)
        cmd_leak(args)
        cmd_mixed(args)
        cmd_watch(args, 'two-readers', TWO_READERS)
        cmd_watch(args, 'flush-evict', FLUSH_EVICT)


if __name__ == '__main__':
    main()
