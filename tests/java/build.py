#!/usr/bin/env python3
"""Build the original Flink queue and a variant with the upstream fix."""
from pathlib import Path
import os
import shutil
import subprocess

root = Path(__file__).resolve().parents[2]
src = root / 'tests/java'
out = root / 'tests/bin/java'
javac = Path(shutil.which('javac')).resolve()
jdk = javac.parent.parent
out.mkdir(parents=True, exist_ok=True)
subprocess.run([os.environ.get('CC', 'cc'), '-shared', '-fPIC',
                '-I' + str(jdk / 'include'), '-I' + str(jdk / 'include/linux'),
                '-I' + str(root / 'include'), str(src / 'ffs_java.c'),
                '-o', str(out / 'libffs_java.so')], check=True)
original = (src / 'flink/FutureCompletingBlockingQueue.java').read_text()
assert original.count('        cond.await();') == 1
for variant in ['buggy', 'fixed']:
    dest = out / variant
    dest.mkdir(exist_ok=True)
    code = original
    if variant == 'fixed':
        code = code.replace('        cond.await();', '''        try {
            cond.await();
        } finally {
            notFull.remove(cond);
        }''')
    queue = dest / 'FutureCompletingBlockingQueue.java'
    queue.write_text(code)
    sources = [queue, src / 'Ffs.java', src / 'Flink37663.java', *sorted((src / 'stubs').rglob('*.java'))]
    subprocess.run([str(javac), '-d', str(dest), *map(str, sources)], check=True)
print(out)
