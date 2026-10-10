//! End-to-end tests.  They need ptrace and a working perf counter, and build
//! the C test programs with `make -C tests/c` (gcc from the dev shell).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Once;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn build_tests() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let st = Command::new("make")
            .args(["-s", "-C"])
            .arg(root().join("tests/c"))
            .status()
            .expect("run make");
        assert!(st.success(), "building tests/c failed");
    });
}

fn prog(name: &str) -> PathBuf {
    build_tests();
    root().join("tests/bin").join(name)
}

fn ffs() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ffs"))
}

fn run(cmd: &mut Command) -> (Output, String) {
    let out = cmd.output().expect("run ffs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out, text)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ffs-test-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn pmu_ok() -> bool {
    ffs()
        .arg("check")
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
fn check_pmu() {
    let (out, text) = run(ffs().arg("check"));
    assert!(out.status.success(), "ffs check failed:\n{text}");
    assert!(text.contains("self-test"), "{text}");
}

#[test]
fn record_and_replay_single_threaded() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("single");
    let trace = d.join("t.json");
    let (out, text) = run(ffs()
        .args(["record", "--policy", "fifo", "-o"])
        .arg(&trace)
        .arg("--")
        .arg(prog("rdtsc_time")));
    assert!(out.status.success(), "record failed:\n{text}");
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert!(out.status.success(), "replay failed:\n{text}");
    assert!(text.contains("replayed"), "{text}");
}

#[test]
fn record_and_replay_multithreaded_with_preemption() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("mt");
    let trace = d.join("t.json");
    let (out, text) = run(ffs()
        .args([
            "record",
            "--seed",
            "3",
            "--preempt-prob",
            "1.0",
            "--max-budget",
            "500",
            "-o",
        ])
        .arg(&trace)
        .arg("--")
        .arg(prog("racy_counter")));
    assert!(out.status.code().is_some(), "{text}");
    let s = std::fs::read_to_string(&trace).unwrap();
    assert!(
        s.contains("\"Preempt\""),
        "trace has no preemption events:\n{text}"
    );
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert!(out.status.code() != Some(5), "replay diverged:\n{text}");
    assert!(text.contains("replayed"), "{text}");
}

#[test]
fn virtual_time_is_deterministic() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("vt");
    let mut outputs = Vec::new();
    for i in 0..2 {
        let trace = d.join(format!("t{i}.json"));
        let (out, _) = run(ffs()
            .args(["record", "--virtual-time", "--policy", "fifo", "-o"])
            .arg(&trace)
            .arg("--")
            .arg(prog("rdtsc_time")));
        assert!(out.status.success());
        outputs.push(String::from_utf8_lossy(&out.stdout).to_string());
    }
    assert!(outputs[0].contains("delta="));
    assert_eq!(outputs[0], outputs[1], "virtual time differed between runs");
}

fn explore_finds(name: &str, expect: &str, iterations: u32, extra: &[&str]) -> (PathBuf, String) {
    let d = tmpdir(name);
    let mut cmd = ffs();
    cmd.args([
        "explore",
        "--iterations",
        &iterations.to_string(),
        "--seed",
        "1",
        "--output-dir",
    ])
    .arg(&d)
    .args(extra)
    .arg("--")
    .arg(prog(name));
    let (out, text) = run(&mut cmd);
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected a failure to be found in {name}:\n{text}"
    );
    assert!(
        text.contains(expect),
        "unexpected failure kind for {name}:\n{text}"
    );
    assert!(
        text.contains("re-execution from the checkpoint: reproduced"),
        "{text}"
    );
    (d.join("fail-1.json"), text)
}

#[test]
fn explore_finds_lost_update_after_bootstrap_checkpoint() {
    if !pmu_ok() {
        return;
    }
    let (trace, text) = explore_finds("bootstrap", "lost update", 400, &[]);
    assert!(text.contains("ffs_checkpoint(\"after-init\")"), "{text}");
    // The saved schedule replays from scratch to the same failure.
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert_eq!(
        out.status.code(),
        Some(2),
        "replay did not reproduce the assertion:\n{text}"
    );
}

#[test]
fn explore_finds_deadlock() {
    if !pmu_ok() {
        return;
    }
    let (trace, _) = explore_finds("deadlock", "deadlock", 200, &[]);
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert_eq!(
        out.status.code(),
        Some(3),
        "replay did not reproduce the deadlock:\n{text}"
    );
}

#[test]
fn explore_finds_order_violation_and_condvar_bug() {
    if !pmu_ok() {
        return;
    }
    explore_finds("order_violation", "used before init", 200, &[]);
    explore_finds("condvar", "consumed from empty queue", 300, &[]);
}

#[test]
fn explore_dfs_backtracks_through_checkpoints() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("dfs");
    let (out, text) = run(ffs()
        .args([
            "explore",
            "--strategy",
            "dfs",
            "--iterations",
            "40",
            "--max-checkpoints",
            "8",
            "--output-dir",
        ])
        .arg(&d)
        .arg("--")
        .arg(prog("bootstrap")));
    assert!(
        out.status.code() == Some(0) || out.status.code() == Some(2),
        "{text}"
    );
    assert!(
        text.contains("explored 40 branches") || text.contains("FAILURE"),
        "{text}"
    );
    assert!(!text.contains("not deterministic"), "{text}");
}

#[test]
fn fork_exec_child_is_traced() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("exec");
    let (out, text) = run(ffs()
        .args(["record", "-o"])
        .arg(d.join("t.json"))
        .arg("--")
        .arg(prog("exec_child")));
    assert!(out.status.success(), "{text}");
    assert!(text.contains("child status=0"), "{text}");
}

#[test]
fn busy_loop_is_preempted() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("spin");
    let (out, text) = run(ffs()
        .args(["record", "-o"])
        .arg(d.join("t.json"))
        .arg("--")
        .arg(prog("spin_flag")));
    assert!(out.status.success(), "{text}");
}

/// Run `name` under `ffs explore` for a few branches; every branch must
/// pass its own checks (the program asserts through `ffs_assert`).
fn explore_passes(name: &str, iterations: u32, extra: &[&str]) -> String {
    let d = tmpdir(name);
    let mut cmd = ffs();
    cmd.args(["explore", "--iterations", &iterations.to_string(), "--seed", "1", "--output-dir"])
        .arg(&d)
        .args(extra)
        .arg("--")
        .arg(prog(name));
    let (out, text) = run(&mut cmd);
    assert_eq!(out.status.code(), Some(0), "{name} failed under exploration:\n{text}");
    assert!(!text.contains("FAIL"), "{text}");
    text
}

#[test]
fn checkpoint_preserves_pids_tids_and_mutex_ownership() {
    if !pmu_ok() {
        return;
    }
    let text = explore_passes("stable_tids", 4, &[]);
    assert!(!text.contains("ids are not stable"), "{text}");
}

#[test]
fn checkpoint_preserves_rseq_registration() {
    if !pmu_ok() {
        return;
    }
    explore_passes("rseq_check", 4, &[]);
}

#[test]
fn checkpoint_privatizes_shared_mappings() {
    if !pmu_ok() {
        return;
    }
    explore_passes("shared_map", 4, &[]);
}

#[test]
fn checkpoint_preserves_pipe_and_eventfd_state() {
    if !pmu_ok() {
        return;
    }
    explore_passes("pipe_state", 4, &[]);
}

#[test]
fn asynchronous_signals_replay_at_the_same_instruction() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("signals");
    let trace = d.join("t.json");
    let (out, text) = run(ffs().args(["record", "--policy", "fifo", "-o"]).arg(&trace).arg("--").arg(prog("signals")));
    assert!(out.status.success(), "record failed:\n{text}");
    let recorded = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(recorded.contains("alarms="), "{text}");
    let s = std::fs::read_to_string(&trace).unwrap();
    assert!(s.contains("\"Signal\""), "no signal was recorded:\n{text}");
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert!(out.status.success(), "replay failed:\n{text}");
    assert!(text.contains("replayed all") || text.contains("replayed"), "{text}");
    assert!(!text.contains("DIVERGED"), "{text}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), recorded, "replay printed a different alarm count:\n{text}");
}

#[test]
fn virtual_time_covers_sleeps_timeouts_and_timers() {
    if !pmu_ok() {
        return;
    }
    let d = tmpdir("vtime");
    let mut outputs = Vec::new();
    for i in 0..2 {
        let trace = d.join(format!("t{i}.json"));
        let start = std::time::Instant::now();
        let (out, text) = run(ffs().args(["record", "--policy", "fifo", "-o"]).arg(&trace).arg("--").arg(prog("vtime")));
        assert!(out.status.success(), "{text}");
        // The program sleeps ~2.5 s of virtual time; none of it is real.
        assert!(start.elapsed() < std::time::Duration::from_millis(1500), "sleeps took real time: {:?}", start.elapsed());
        outputs.push(String::from_utf8_lossy(&out.stdout).to_string());
    }
    assert_eq!(outputs[0], outputs[1], "virtual time differed between runs");
    let o = &outputs[0];
    assert!(o.contains("sleep(1) -> 1050 ms"), "{o}");
    assert!(o.contains("ETIMEDOUT"), "{o}");
    assert!(o.contains("poll(300ms) -> 0"), "{o}");
    assert!(o.contains("usr1s=1 siginfo_ok=1"), "{o}");
    assert!(o.contains("remaining=5"), "{o}");
    let alarms: i32 = o.lines().find_map(|l| l.strip_prefix("alarms=")).unwrap().trim().parse().unwrap();
    assert!(alarms >= 10, "interval timer fired only {alarms} times:\n{o}");
    let (out, text) = run(ffs().arg("replay").arg(d.join("t0.json")));
    assert!(out.status.success() && !text.contains("DIVERGED"), "{text}");
}

#[test]
fn watchpoints_reach_straight_line_races() {
    if !pmu_ok() {
        return;
    }
    // Explicit watchpoints on the two globals (the random policy needs
    // well under 200 schedules for this; PCT is much weaker here).
    let (trace, text) = explore_finds("straight_line", "a == 1 && b == 0", 800, &["--policy", "random", "--watch", "a,b"]);
    let (out, text2) = run(ffs().arg("replay").arg(&trace));
    assert_eq!(out.status.code(), Some(2), "replay did not reproduce:\n{text}\n{text2}");
    // Sampled watchpoints (the default) find it too.
    explore_finds("straight_line", "a == 1 && b == 0", 1500, &["--policy", "random"]);
}

#[test]
fn checkpoint_recreates_socketpairs_with_their_data() {
    if !pmu_ok() {
        return;
    }
    explore_passes("socket_state", 4, &[]);
}

#[test]
fn checkpoint_preserves_the_process_tree() {
    if !pmu_ok() {
        return;
    }
    let text = explore_passes("fork_tree", 4, &[]);
    assert!(!text.contains("not preserved"), "{text}");
}

#[allow(dead_code)]
fn exists(p: &Path) -> bool {
    p.exists()
}
