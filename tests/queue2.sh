#!/usr/bin/env bash
# Follow-ups after queue.sh: reruns with adjusted workloads (4 sessions).
cd "$(dirname "$0")"
cprog/run.py cprog/programs.tsv --only '^ck_ring' --iterations 100 --timeout 1800 -j 4 --out cprog/results/programs-ck3 > cprog/results-programs-ck3.log 2>&1
cprog/run.py cprog/programs.tsv --only '^sqlite_checkpoint_starvation' --iterations 20 --timeout 1800 -j 2 --out cprog/results/programs-sqlite3s > cprog/results-programs-sqlite3s.log 2>&1
cprog/run.py cprog/programs.tsv --only '^tbb_task_arena' --iterations 30 --timeout 1800 -j 1 --out cprog/results/programs-tbb-rerun > cprog/results-programs-tbb-rerun.log 2>&1
echo queue2-done
