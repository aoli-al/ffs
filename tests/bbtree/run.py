#!/usr/bin/env python3
"""Test bbtree (https://github.com/XiangpengHao/bbtree) under ffs.

    ./run.py setup [--bbtree ~/repos/bbtree-src/bbtree]   # wire + build the harness
    ./run.py list                                           # the scenarios
    ./run.py explore [SCENARIO ...] --iterations 2000 --seeds 1 2
    ./run.py explore separator_lookups_race_a_merge --watch root --watch-hold 0.5
    ./run.py native [SCENARIO ...] --runs 10000             # plain stress, no ffs
    ./run.py replay out/<scenario>/<tag>/fail-1.json
    ./run.py all

The harness (ffs_tests.rs) is bbtree's own Shuttle scenarios
(src/map/shuttle_tests.rs) on real threads: it is symlinked into the
checkout as src/ffs_tests.rs and compiled into the crate's unit-test binary
with `--cfg ffs` (setup also widens two `cfg(shuttle)` gates so the tests
can read the tree's shape).  Each scenario sets a tree up, calls
`ffs_checkpoint`, races its threads and checks the tree; `ffs explore` runs
the race under many schedules.
"""
import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
FFS = HERE.parents[1] / 'target/release/ffs'
MODULE = HERE / 'ffs_tests.rs'
BINARY_FILE = HERE / 'out/test-binary'
DEFAULT_BBTREE = os.path.expanduser('~/repos/bbtree-src/bbtree')
BUILD_ENV = {
    'RUSTFLAGS': '--cfg ffs --check-cfg cfg(ffs)',
    'CARGO_TARGET_DIR': 'target/ffs',
    # Release speed (Shuttle's CI builds release too), with the checks on.
    'CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS': 'true',
    'CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS': 'true',
    'CARGO_PROFILE_RELEASE_DEBUG': '1',
}


def sh(cmd, **kw):
    print('+', ' '.join(str(c) for c in cmd), file=sys.stderr)
    return subprocess.run([str(c) for c in cmd], **kw)


def patch(path, old, new):
    text = path.read_text()
    if new in text:
        return
    assert old in text, f'{path}: could not find {old!r}'
    path.write_text(text.replace(old, new))
    print(f'[run] patched {path}', file=sys.stderr)


def setup(args):
    bb = Path(args.bbtree).resolve()
    link = bb / 'src/ffs_tests.rs'
    if link.is_symlink() or link.exists():
        link.unlink()
    link.symlink_to(MODULE)
    patch(bb / 'src/lib.rs', 'pub mod unsync;', 'pub mod unsync;\n#[cfg(all(test, ffs))]\nmod ffs_tests;')
    patch(bb / 'src/map/mod.rs', '#[cfg(shuttle)]', '#[cfg(any(shuttle, ffs))]')
    patch(bb / 'src/tree/latched/mod.rs', '#[cfg(all(test, shuttle))]', '#[cfg(all(test, any(shuttle, ffs)))]')
    env = dict(os.environ, **BUILD_ENV)
    r = sh(['cargo', 'test', '--release', '--no-run', '--lib', '--message-format=json'],
           cwd=bb, env=env, capture_output=True, text=True)
    binary = None
    for line in r.stdout.splitlines():
        try:
            msg = json.loads(line)
        except ValueError:
            continue
        if msg.get('reason') == 'compiler-artifact' and msg.get('executable') and msg['target']['name'] == 'bbtree':
            binary = msg['executable']
    if r.returncode or not binary:
        sys.stderr.write(r.stderr)
        sys.exit('[run] build failed')
    BINARY_FILE.parent.mkdir(parents=True, exist_ok=True)
    BINARY_FILE.write_text(binary)
    print(f'[run] test binary: {binary}', file=sys.stderr)
    if not FFS.exists():
        sys.exit(f'[run] build ffs first: {FFS} is missing')


BINARY_OVERRIDE = None


def binary():
    if BINARY_OVERRIDE:
        return Path(BINARY_OVERRIDE)
    if not BINARY_FILE.exists():
        sys.exit('[run] run `./run.py setup` first')
    return Path(BINARY_FILE.read_text().strip())


def scenarios():
    r = sh([binary(), '--list'], capture_output=True, text=True)
    names = [line.split(': ')[0].split('::')[-1] for line in r.stdout.splitlines() if line.startswith('ffs_tests::')]
    return [n for n in names if n != 'shapes_are_as_the_tests_assume']


def harness_argv(scenario):
    return [binary(), '--exact', f'ffs_tests::{scenario}', '--test-threads=1', '--nocapture']


FAIL_RE = re.compile(r'^\[ffs\] branch (\d+): FAILURE (.*?)(?: after \d+ steps.*)?$', re.M)
PANIC_RE = re.compile(r"^thread '([^']+)' \(\d+\) panicked at ([^\n]+):\n([^\n]+)", re.M)
EXPLORED_RE = re.compile(r'explored (\d+) branches, (\d+) failing')


def explore(args, scenario, seed):
    tag = f'{args.policy}-w{args.watch.replace(",", "+")}-h{args.watch_hold}-s{seed}'
    d = HERE / 'out' / (args.tag + scenario)
    d.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, FFS_BBTREE_WATCH=args.watch, FFS_BBTREE_SEED=str(seed))
    if args.ops:
        env['FFS_BBTREE_OPS'] = str(args.ops)
    cmd = [FFS, 'explore', '--iterations', args.iterations, '--seed', seed, '--keep-going',
           '--policy', args.policy, '--watch-hold', args.watch_hold, '--max-depth', args.max_depth,
           '--output-dir', d / tag]
    if args.watch != 'none':
        cmd += ['--watch-count', '0']  # the harness's watchpoints, not sampled globals
    cmd += ['--', *harness_argv(scenario)]
    r = sh(cmd, env=env, capture_output=True, text=True)
    (d / f'{tag}.log').write_text(r.stderr + r.stdout)
    failures = FAIL_RE.findall(r.stderr)
    explored = EXPLORED_RE.search(r.stderr)
    print(f'[run] {scenario} {tag}: {explored.group(0) if explored else "? (" + r.stderr.strip().splitlines()[-1][:100] + ")"}')
    kinds = {}
    for _, what in failures:
        kinds[re.sub(r'\d+', 'N', what)] = kinds.get(re.sub(r'\d+', 'N', what), 0) + 1
    for k, n in sorted(kinds.items(), key=lambda kv: -kv[1]):
        print(f'      {n:4d} x {k}')
    bound = r.stderr.count('depth bound (')
    if bound:
        print(f'      {bound:4d} x depth bound (a spinner never let go: not counted as failing)')
    panics = {}
    for _, where, msg in PANIC_RE.findall(r.stderr):
        if 'expected panic' in msg:
            continue  # panicking_closures_let_other_threads_through panics on purpose
        key = f'{where.split("/src/")[-1]}: {msg.strip()[:90]}'
        panics[key] = panics.get(key, 0) + 1
    for k, n in sorted(panics.items(), key=lambda kv: -kv[1]):
        print(f'      {n:4d} panics: {k}')
    return explored, kinds


def cmd_explore(args):
    names = args.scenarios or scenarios()
    total = failing = 0
    for scenario in names:
        for seed in args.seeds:
            explored, _ = explore(args, scenario, seed)
            if explored:
                total += int(explored.group(1))
                failing += int(explored.group(2))
    print(f'[run] total: {total} branches, {failing} failing')


def cmd_native(args):
    """Plain repeated native runs, for comparison (no ffs): the whole
    suite per run, so that the scenarios' own threads are the only load."""
    names = args.scenarios or scenarios()
    fails = {name: 0 for name in names}
    hangs = 0
    for _ in range(args.runs):
        try:
            r = subprocess.run([str(binary()), *[f'ffs_tests::{n}' for n in names], '--exact', '--test-threads=1'],
                               capture_output=True, text=True, timeout=20)
        except subprocess.TimeoutExpired as e:
            # A broken latch protocol can also deadlock: the run is counted
            # against the scenario that was running.
            hangs += 1
            out = (e.stdout or b'').decode(errors='replace')
            started = re.findall(r'^test ffs_tests::(\w+) \.\.\. ', out, re.M)
            if started and started[-1] in fails:
                fails[started[-1]] += 1
            continue
        for name in re.findall(r'^test ffs_tests::(\w+) \.\.\. FAILED', r.stdout, re.M):
            fails[name] += 1
            if fails[name] == 1:
                (HERE / 'out' / (args.tag + name)).mkdir(parents=True, exist_ok=True)
                (HERE / 'out' / (args.tag + name) / 'native-failure.log').write_text(r.stdout + r.stderr)
    for name in names:
        print(f'[run] native {name}: {fails[name]}/{args.runs} failing')
    if hangs:
        print(f'[run] native: {hangs} of {args.runs} runs hung (killed after 20 s)')


def cmd_replay(args):
    r = sh([FFS, 'replay', *args.replay_opts, args.trace])
    sys.exit(r.returncode)


def main():
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument('--bbtree', default=DEFAULT_BBTREE)
    common.add_argument('--iterations', type=int, default=500)
    common.add_argument('--seeds', type=int, nargs='+', default=[1])
    common.add_argument('--policy', default='mix')
    common.add_argument('--watch', default='none', help='none | root | last | root,last (FFS_BBTREE_WATCH)')
    common.add_argument('--watch-hold', type=float, default=0.0)
    common.add_argument('--max-depth', type=int, default=200000)
    common.add_argument('--ops', type=int, default=0, help='operations per model thread (FFS_BBTREE_OPS)')
    common.add_argument('--runs', type=int, default=1000)
    common.add_argument('--binary', help='a test binary other than the one setup built (a mutant)')
    common.add_argument('--tag', default='', help='a prefix for the output directory')
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest='cmd', required=True)
    sub.add_parser('setup', parents=[common])
    sub.add_parser('list', parents=[common])
    for name in ['explore', 'native', 'all']:
        p = sub.add_parser(name, parents=[common])
        p.add_argument('scenarios', nargs='*')
    rp = sub.add_parser('replay', parents=[common])
    rp.add_argument('trace')
    rp.add_argument('replay_opts', nargs='*')
    args = ap.parse_args()
    global BINARY_OVERRIDE
    BINARY_OVERRIDE = args.binary
    if args.cmd == 'setup':
        setup(args)
    elif args.cmd == 'list':
        print('\n'.join(scenarios()))
    elif args.cmd == 'explore':
        cmd_explore(args)
    elif args.cmd == 'native':
        cmd_native(args)
    elif args.cmd == 'replay':
        cmd_replay(args)
    elif args.cmd == 'all':
        setup(args)
        cmd_explore(args)
        args.watch, args.watch_hold = 'root', 0.5
        cmd_explore(args)


if __name__ == '__main__':
    main()
