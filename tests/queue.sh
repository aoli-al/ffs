#!/usr/bin/env bash
# One serial queue, at most 4 ffs sessions at a time (user request).
cd "$(dirname "$0")"
J=4
cprog/run.py cprog/programs.tsv --only '^(cq|mi|sqlite|tbb)_' --iterations 100 --timeout 1800 -j $J --out cprog/results/programs-cxx --resume > cprog/results-programs-cxx.log 2>&1
cprog/run.py cprog/programs.tsv --only '^ck_' --iterations 100 --timeout 1800 -j $J --out cprog/results/programs-ck2 --resume > cprog/results-programs-ck2.log 2>&1
cprog/run.py cprog/programs-cxx2.tsv --iterations 100 --timeout 1800 -j $J --out cprog/results/programs-cxx2 --resume > cprog/results-programs-cxx2.log 2>&1
cd crates
./run.py run kanal --bin sync_test --bin async_test --iterations 100 --timeout 1800 -j $J --out results/more/kanal2 --resume > results-more-kanal2.log 2>&1
for c in moka tokio-util futures-rs rayon; do ./run.py run $c --iterations 100 --timeout 1800 -j $J --out results/more/$c --resume > results-more-$c.log 2>&1; done
./run.py run tokio --bin sync_notify --bin sync_notify_owned --bin sync_broadcast --bin sync_broadcast_weak --bin sync_mpsc --bin sync_mpsc_weak --bin sync_semaphore --bin sync_semaphore_owned --bin sync_oneshot --bin sync_watch --bin sync_rwlock --bin sync_mutex --bin sync_mutex_owned --bin sync_barrier --bin sync_once_cell --bin sync_set_once --bin rt_threaded --bin rt_common --bin rt_handle --bin rt_spawn_blocking --bin rt_blocking_thread_exhaust --bin task_join_set --bin task_abort --bin task_local_set --bin task_blocking --bin io_async_fd --bin time_rt --bin time_sleep --bin time_timeout --bin time_interval --bin tokio --iterations 5000 --timeout 3600 -j $J --out results/x5000/tokio --resume > results-x5000-tokio.log 2>&1
./run.py run crossbeam --bin select --bin zero --bin array --bin list --bin ready --bin select_macro --bin golang --bin mpsc --bin steal --bin injector --bin fifo --bin lifo --bin seg_queue --bin array_queue --bin base --bin map --bin set --bin sharded_lock --bin parker --bin wait_group --bin atomic_cell --bin crossbeam_epoch --iterations 5000 --timeout 3600 -j $J --out results/x5000/crossbeam --resume > results-x5000-crossbeam.log 2>&1
for c in parking_lot dashmap papaya concurrent-queue flume; do ./run.py run $c --iterations 5000 --timeout 3600 -j $J --out results/x5000/$c --resume > results-x5000-$c.log 2>&1; done
echo queue-done
