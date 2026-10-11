#!/usr/bin/env python3
"""Group results.tsv by status and by the ffs message that explains it."""
import re, sys
from collections import Counter, defaultdict

path = sys.argv[1] if len(sys.argv) > 1 else 'out/results.tsv'
rows = [l.rstrip('\n').split('\t') for l in open(path) if l.strip()]
by = defaultdict(list)
for name, st, *rest in rows:
    detail = rest[0] if rest else ''
    # Normalise the detail into a "reason" key.
    key = detail
    key = re.sub(r'\[ffs\] ', '', key)
    key = re.sub(r'T\d+', 'T', key)
    key = re.sub(r'@\d+:0x[0-9a-f]+', '', key)
    key = re.sub(r'step \d+', 'step N', key)
    key = re.sub(r'\d+ of \d+ steps', 'N of N steps', key)
    key = re.sub(r'= -?\d+', '= R', key)
    key = re.sub(r'exit=\d+ ', '', key)
    key = re.sub(r'\(\d+\)', '', key)
    key = re.sub(r'task \d+', 'task N', key)
    key = re.sub(r'\d{3,}', 'N', key)
    by[st].append((name, key[:160]))
order = sorted(by, key=lambda s: -len(by[s]))
for st in order:
    items = by[st]
    print(f'\n## {st} ({len(items)})')
    if st == 'PASS':
        continue
    groups = defaultdict(list)
    for name, key in items:
        groups[key].append(name)
    for key, names in sorted(groups.items(), key=lambda kv: -len(kv[1])):
        print(f'  [{len(names)}] {key}')
        print('      ' + ' '.join(sorted(names)))
