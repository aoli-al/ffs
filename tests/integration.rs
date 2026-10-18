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

/// Build the Java harnesses (tests/java/build.py) once; None without a JDK.
fn build_java() -> Option<PathBuf> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static ONCE: Once = Once::new();
    static OK: AtomicBool = AtomicBool::new(false);
    ONCE.call_once(|| {
        if Command::new("javac").arg("-version").output().is_err() {
            eprintln!("javac not found; skipping Java tests");
            return;
        }
        let st = Command::new("python3")
            .arg(root().join("tests/java/build.py"))
            .status()
            .expect("run build.py");
        assert!(st.success(), "building tests/java failed");
        OK.store(true, Ordering::SeqCst);
    });
    if OK.load(Ordering::SeqCst) {
        Some(root().join("tests/bin/java"))
    } else {
        None
    }
}

/// Command line of a Java harness class under `variant` (buggy/fixed).
fn java_args(java: &Path, variant: &str, class: &str) -> Vec<String> {
    vec![
        "java".into(),
        // The JVM lists /tmp/hsperfdata_<user> at startup: live state.
        "-XX:-UsePerfData".into(),
        format!("-Djava.library.path={}", java.display()),
        "-cp".into(),
        java.join(variant).display().to_string(),
        class.into(),
    ]
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

#[test]
fn shadow_replay_checks_live_inputs() {
    let d = tmpdir("replay-inputs");
    let input = d.join("input");
    let trace = d.join("trace.json");
    for mode in ["read", "readv", "offset", "stat"] {
        std::fs::write(&input, b"original").unwrap();
        let (out, text) = run(ffs().args(["record", "--ticks", "none", "--policy", "fifo", "-o"])
            .arg(&trace).arg("--").arg(prog("replay_inputs")).arg(mode).arg(&input));
        assert!(out.status.success(), "record {mode}: {text}");
        let (out, text) = run(ffs().args(["replay", "--ticks", "none"]).arg(&trace));
        assert!(out.status.success(), "unchanged replay {mode}: {text}");
        // Same return count, different contents: the default check accepts
        // the live result; the optional buffer check catches the changed bytes.
        std::fs::write(&input, b"modified").unwrap();
        let (out, text) = run(ffs().arg("replay").arg(&trace));
        assert!(out.status.success(), "matching live return values {mode}: {text}");
        if mode != "stat" {
            assert!(text.contains("modified"), "replay did not use live bytes: {text}");
        }
        let (out, text) = run(ffs().args(["replay", "--verify-outputs"]).arg(&trace));
        assert_eq!(out.status.code(), Some(5), "changed input {mode}: {text}");
        assert!(text.contains("syscall output bytes"), "{text}");
        // A shorter file changes read's scalar return value as well.
        if mode != "stat" {
            std::fs::write(&input, b"x").unwrap();
            let (out, text) = run(ffs().arg("replay").arg(&trace));
            assert_eq!(out.status.code(), Some(5), "changed read count {mode}: {text}");
            assert!(text.contains("= 8") && text.contains("= 1"), "{text}");
        }
        // A failed live open/stat must not be replaced with recorded success.
        std::fs::remove_file(&input).unwrap();
        let (out, text) = run(ffs().arg("replay").arg(&trace));
        assert_eq!(out.status.code(), Some(5), "missing input {mode}: {text}");
        assert!(text.contains("= -2"), "{text}");
    }
}

#[test]
fn async_memory_interfaces_are_refused() {
    let d = tmpdir("refuse-async");
    let trace = d.join("trace.json");
    let (out, text) = run(ffs().args(["record", "--ticks", "none", "--policy", "fifo", "-o"])
        .arg(&trace).arg("--").arg(prog("replay_inputs")).arg("async"));
    assert!(out.status.success(), "{text}");
    let (out, text) = run(ffs().args(["replay", "--ticks", "none"]).arg(&trace));
    assert!(out.status.success(), "{text}");
}


#[test]
fn replay_rejects_changed_returns_buffers_and_incomplete_execution() {
    let d = tmpdir("replay-checks");
    let trace = d.join("trace.json");
    let bad = d.join("bad.json");
    let (out, text) = run(ffs().args(["record", "--ticks", "none", "--policy", "fifo", "-o"])
        .arg(&trace).arg("--").arg(prog("replay_inputs")).arg("pipe"));
    assert!(out.status.success(), "{text}");
    // The source and slice settings come from the trace without CLI overrides.
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert!(out.status.success(), "{text}");
    let original: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
    let mut changed = original.clone();
    let write = changed["steps"].as_array_mut().unwrap().iter_mut()
        .find(|s| s["event"]["SyscallExit"]["nr"] == 1).unwrap();
    write["event"]["SyscallExit"]["ret"] = serde_json::json!(7);
    std::fs::write(&bad, serde_json::to_vec(&changed).unwrap()).unwrap();
    let (out, text) = run(ffs().arg("replay").arg(&bad));
    assert_eq!(out.status.code(), Some(5), "return mismatch: {text}");
    let mut changed = original.clone();
    let read = changed["steps"].as_array_mut().unwrap().iter_mut()
        .find(|s| s["event"]["SyscallExit"]["nr"] == 0).unwrap();
    // Small buffers are stored byte for byte, larger ones as (length, hash).
    let out0 = &mut read["effects"]["out"][0];
    if out0["bytes"].as_array().is_some_and(|a| !a.is_empty()) {
        out0["bytes"][0] = serde_json::json!(0);
    } else {
        out0["digest"][1] = serde_json::json!(1);
    }
    std::fs::write(&bad, serde_json::to_vec(&changed).unwrap()).unwrap();
    let (out, text) = run(ffs().args(["replay", "--verify-outputs"]).arg(&bad));
    assert_eq!(out.status.code(), Some(5), "buffer mismatch: {text}");
    assert!(text.contains("syscall output bytes"), "{text}");
    let (out, text) = run(ffs().args(["replay", "--max-steps", "1"]).arg(&trace));
    assert_eq!(out.status.code(), Some(5), "incomplete replay: {text}");
    assert!(!text.contains("replayed all"), "{text}");
    // Old schedules remain readable, with an explicit opt-out of return checks.
    let mut legacy = original;
    legacy["version"] = serde_json::json!(1);
    legacy.as_object_mut().unwrap().remove("execution");
    for step in legacy["steps"].as_array_mut().unwrap() {
        step.as_object_mut().unwrap().remove("effects");
    }
    std::fs::write(&bad, serde_json::to_vec(&legacy).unwrap()).unwrap();
    let (out, text) = run(ffs().args(["replay", "--ticks", "none", "--lenient"]).arg(&bad));
    assert!(out.status.success(), "legacy replay: {text}");
}

#[test]
fn timerfd_runs_on_the_virtual_clock() {
    build_tests();
    let d = tmpdir("timerfd");
    let (out, text) = run(ffs().args(["record", "--ticks", "none", "--policy", "fifo", "-o"])
        .arg(d.join("trace.json")).arg("--").arg(prog("replay_inputs")).arg("timerfd"));
    assert!(out.status.success(), "{text}");
    // A periodic timerfd read through epoll: 5 expirations of 20 ms take
    // 100 virtual ms, and the recording replays.
    let trace = d.join("loop.json");
    let (out, text) = run(ffs().args(["record", "--seed", "1", "-o"]).arg(&trace).arg("--").arg(prog("timerfd_loop")));
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("OK 5 wakeups 5 expirations"), "{text}");
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("replayed all"), "{text}");
}

#[test]
fn syscall_only_recordings_repeat_without_host_time_slices() {
    let d = tmpdir("same-seed");
    let input = d.join("input");
    std::fs::write(&input, b"original").unwrap();
    let trace = d.join("trace.json");
    let mut previous = None;
    for _ in 0..3 {
        let (out, text) = run(ffs().args([
            "record", "--ticks", "none", "--slice-cap-ms", "0",
            "--block-grace-us", "0", "--seed", "42", "-o",
        ]).arg(&trace).arg("--").arg(prog("replay_inputs")).arg("read").arg(&input));
        assert!(out.status.success(), "{text}");
        let t: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace).unwrap()).unwrap();
        let steps = t["steps"].clone();
        assert!(!steps.as_array().unwrap().iter().any(|s| s["event"] == "Slice"));
        if let Some(previous) = previous {
            assert_eq!(steps, previous, "same-seed syscall-only run changed");
        }
        previous = Some(steps);
    }
}


#[test]
fn compat_syscall_abi_is_rejected_before_virtualization() {
    let d = tmpdir("compat-syscall");
    let (out, text) = run(ffs().args(["record", "--ticks", "none", "--policy", "fifo", "-o"])
        .arg(d.join("trace.json")).arg("--").arg(prog("replay_inputs")).arg("compat"));
    assert_eq!(out.status.code(), Some(70), "{text}");
    assert!(text.contains("unsupported syscall ABI"), "{text}");
}


#[test]
fn shadow_replay_verifies_virtual_time_and_process_ids() {
    let d = tmpdir("shadow-virtual");
    let trace = d.join("trace.json");
    let (record, text) = run(ffs().args(["record", "--ticks", "none", "--policy", "fifo", "-o"])
        .arg(&trace).arg("--").arg(prog("replay_inputs")).arg("virtual"));
    assert!(record.status.success(), "{text}");
    let (replay, text) = run(ffs().args(["replay", "--verify-outputs"]).arg(&trace));
    assert!(replay.status.success(), "virtualized outputs differ: {text}");
    assert_eq!(record.stdout, replay.stdout, "time and pid/tid must match");
}

#[test]
fn shadow_replay_preserves_kernel_semantics() {
    let d = tmpdir("kernel-semantics");
    // Reproduce Nix's long TMPDIR without relying on the surrounding shell.
    let long_tmp = d.join("a-temporary-directory-longer-than-the-cpu-online-path");
    std::fs::create_dir_all(&long_tmp).unwrap();
    for mode in ["waits", "clone_parent", "tsc", "cpu", "vm_write"] {
        let trace = d.join(format!("{mode}.json"));
        let (record, text) = run(ffs().env("TMPDIR", &long_tmp).args([
            "record", "--ticks", "none", "--slice-cap-ms", "0", "--policy", "fifo", "-o",
        ]).arg(&trace).arg("--").arg(prog("kernel_semantics")).arg(mode));
        assert!(record.status.success(), "{mode}: {text}");
        let (replay, text) = run(ffs().env("TMPDIR", &long_tmp)
            .args(["replay", "--verify-outputs"]).arg(&trace));
        assert!(replay.status.success(), "{mode}: {text}");
        assert_eq!(record.stdout, replay.stdout, "{mode}");
    }
}

/// FLINK-37663: the real `FutureCompletingBlockingQueue` loses a putter's
/// wakeup when `wakeUpPuttingThread` signals a condition still queued in
/// `notFull`.  The JVM (JIT, GC and safepoint signals, 16 threads at the
/// checkpoint) runs under ffs; the failing schedule replays from a cold
/// start; the upstream fix passes.
#[test]
fn explore_finds_flink_37663_in_the_jvm() {
    let Some(java) = build_java() else { return };
    let d = tmpdir("flink37663");
    let mut cmd = ffs();
    cmd.args(["explore", "--iterations", "50", "--seed", "1", "--output-dir"])
        .arg(&d)
        .arg("--")
        .args(java_args(&java, "buggy", "Flink37663"));
    let (out, text) = run(&mut cmd);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("FLINK-37663: producer B stranded"), "{text}");
    assert!(text.contains("re-execution from the checkpoint: reproduced"), "{text}");

    let (out, text) = run(ffs().arg("replay").arg(d.join("fail-1.json")));
    assert_eq!(out.status.code(), Some(2), "cold replay of the JVM trace:\n{text}");
    assert!(text.contains("ASSERTION FAILED: FLINK-37663"), "{text}");
    assert!(text.contains("replayed all"), "{text}");

    let d = tmpdir("flink37663-fixed");
    let mut cmd = ffs();
    cmd.args(["explore", "--iterations", "30", "--seed", "1", "--output-dir"])
        .arg(&d)
        .arg("--")
        .args(java_args(&java, "fixed", "Flink37663"));
    let (out, text) = run(&mut cmd);
    assert_eq!(out.status.code(), Some(0), "fixed queue failed under exploration:\n{text}");
}

/// A deadlock next to a periodic timer thread (a JVM always has some): the
/// process is never "all blocked", so the deadlock is inferred from a long
/// stretch of timer-only wakeups.  A long one-shot sleep beside the same
/// timer thread must not be mistaken for one, and the recorded outcome
/// must replay.
#[test]
fn deadlock_is_detected_despite_timer_threads() {
    build_tests();
    let (trace, text) = explore_finds("deadlock_timer", "deadlock", 30, &[]);
    assert!(text.contains("blocked threads [1, 3, 4]"), "{text}");
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("DEADLOCK: T1 T3 T4 blocked with no timeout"), "{text}");
    assert!(text.contains("only the timer-driven threads T2 ran"), "{text}");

    let d = tmpdir("deadlock_timer_sleep");
    let mut cmd = ffs();
    cmd.args(["explore", "--iterations", "3", "--seed", "1", "--output-dir"])
        .arg(&d)
        .arg("--")
        .arg(prog("deadlock_timer"))
        .arg("sleep");
    let (out, text) = run(&mut cmd);
    assert_eq!(out.status.code(), Some(0), "a 90 s sleep is not a deadlock:\n{text}");
    assert!(text.contains("woke up"), "{text}");
}

/// The same Flink bug with an untimed wait is a deadlock inside a JVM with
/// its compiler, GC and periodic threads alive.
#[test]
fn deadlock_is_detected_in_the_jvm() {
    let Some(java) = build_java() else { return };
    let d = tmpdir("flink37663-deadlock");
    let mut cmd = ffs();
    cmd.args(["explore", "--iterations", "50", "--seed", "1", "--output-dir"])
        .arg(&d)
        .arg("--")
        .args(java_args(&java, "buggy", "Flink37663"))
        .arg("notimeout");
    let (out, text) = run(&mut cmd);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("FAILURE deadlock"), "{text}");
    assert!(text.contains("re-execution from the checkpoint: reproduced"), "{text}");
}

/// `--root vtime:SECS` and `--root steps:N` place the checkpoint without
/// any call into ffs; `record --timeline` prints the virtual times to
/// choose from.
#[test]
fn exploration_roots_at_a_virtual_time_without_the_api() {
    build_tests();
    let (out, text) = run(ffs()
        .args(["record", "--timeline", "-o"])
        .arg(tmpdir("timeline").join("t.json"))
        .arg("--")
        .arg(prog("deadlock_timer"))
        .arg("sleep"));
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("timeline (virtual s): T1 at 0.000, T2 at 0.0"), "{text}");
    assert!(text.contains("first write to stdout/stderr at 90."), "{text}");

    let d = tmpdir("root-vtime");
    let mut cmd = ffs();
    cmd.args(["explore", "--root", "vtime:0.00001", "--iterations", "30", "--seed", "1", "--output-dir"])
        .arg(&d)
        .arg("--")
        .arg(prog("deadlock_timer"));
    let (out, text) = run(&mut cmd);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("exploration root after"), "{text}");
    assert!(text.contains("FAILURE deadlock"), "{text}");
}

/// A preemption signal pending during `getdents64` makes the kernel return
/// a single entry; the replay must make the same signal pending at the
/// same syscall (the trace shows the preemption right after the exit).
#[test]
fn signal_shortened_syscall_results_replay() {
    build_tests();
    let d = tmpdir("dents");
    let trace = d.join("t.json");
    let (out, text) = run(ffs()
        .args(["record", "--max-insn-steps", "0", "--preempt-prob", "1", "--max-budget", "3000", "--seed", "7", "-o"])
        .arg(&trace)
        .arg("--")
        .arg(prog("dents_loop"))
        .arg("20000"));
    assert_eq!(out.status.code(), Some(0), "{text}");
    let (_, shown) = run(ffs().arg("show").arg(&trace));
    assert!(shown.contains("-> getdents64 = 24"), "no signal-shortened getdents64 in the recording");
    let (out, text) = run(ffs().arg("replay").arg(&trace));
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("replayed all"), "{text}");
}
