//! Binary-level tests of the checkpointed sweep driver (spec v11): per-run
//! saves, resume, `--fresh`, the interactive prompt, strict abort rules,
//! `--dry-run`, run-major order, signals, panics, `--stop-at`/`--start-at`.
//!
//! Every test drives the real `dpds_simulation` binary in its own scratch
//! directory with tiny seeded configs. Expectations are computed
//! independently of the binary: the expected CSVs come from
//! `lib::simulation::run_simulation` (seed + run), never from a reference
//! sweep of the binary itself or from the checkpoint code under test.
//!
//! The library half of the checkpoint API (`lib::checkpoint`) is specified
//! at the top of `checkpoint.rs`; these tests use it to write and read
//! checkpoint files.
//!
//! # Test-facing CLI fixed by these tests
//!
//! ```text
//! dpds_simulation [--resume | --fresh] [--dry-run] [--stop-at HH:MM] [--start-at HH:MM]
//! ```
//!
//! Reads `simulation_configs.json` from the working directory, writes the
//! per-config CSVs and `results/.checkpoint.json` (JSON shape: see
//! `checkpoint.rs`). Tests look at the combined stdout + stderr only for the
//! strings listed here; everything else in the wording is free.
//!
//! * Start-up with an existing checkpoint (after parsing the arguments and
//!   validating the checkpoint, before anything runs):
//!   - the checkpoint must load strictly and its `fingerprint` must equal
//!     `lib::checkpoint::fingerprint(configs)`; otherwise exit non-zero, name
//!     the file (`.checkpoint.json`) and `--fresh` in the message, leave the
//!     file byte-identical and run nothing (even with `--resume`);
//!   - print a summary containing `<completed>/<total>` runs (e.g. `4/18`);
//!   - `--resume`: resume without prompting; `--fresh`: delete the
//!     checkpoint and start over without prompting; both: exit non-zero
//!     naming `--resume` and `--fresh` before anything is written;
//!   - neither flag, stdin not a TTY and `DPDS_SIM_FORCE_PROMPT` unset:
//!     exit non-zero, message names `--resume` and `--fresh`, nothing is
//!     changed;
//!   - neither flag and (stdin is a TTY or `DPDS_SIM_FORCE_PROMPT=1`): prompt
//!     on stdout with the exact text `Resume it? [y/n]`; `y` resumes; `n`
//!     asks `Really delete the checkpoint and start fresh? [y/n]`; `y` there
//!     starts fresh, a second `n` exits non-zero with the checkpoint
//!     untouched; any other input re-prompts the same question; EOF on stdin
//!     (no answer) exits non-zero with the checkpoint untouched (it must not
//!     loop forever or guess).
//!   - no checkpoint: never prompts; just starts.
//! * `--dry-run`: print the plan and exit 0, writing nothing, never
//!   prompting, never waiting. The plan names every `csv_output_path`, the
//!   number of jobs, the estimate of simulated packets as a plain decimal
//!   integer (sum over runs of `floor(duration_ns / iat_ns) + 1` arrivals, the
//!   count `model.rs` calls `expected_arrivals`), and, if a checkpoint
//!   exists, `<completed>/<total>`.
//! * `--stop-at HH:MM` / `--start-at HH:MM`: local time of day, `H:MM`
//!   accepted, hour 0-23, minute 0-59, nothing else (no seconds, no
//!   suffix). The time means its NEXT occurrence: a time that already
//!   passed today (or equals now) is tomorrow. Invalid values (and a flag
//!   without value) exit non-zero naming the flag, before anything is
//!   written. Order at start-up: arguments, checkpoint validation and
//!   prompts first, THEN the `--start-at` wait, then the jobs.
//!   `--stop-at` reached: stop handing out jobs and behave like a first
//!   Ctrl+C (non-zero exit, message names `--resume`).
//! * Test seam for the clock: `DPDS_SIM_FAKE_NOW=HH:MM:SS` makes the process
//!   treat that local time of day as "now" at start-up (time then advances
//!   normally). Needed so `--stop-at`/`--start-at` tests are deterministic.
//! * First SIGINT/SIGTERM: stop handing out jobs, finish and checkpoint the
//!   in-flight runs, write the CSVs of complete configs, message names
//!   `--resume`, exit with a non-zero exit CODE (not death by signal).
//!   Second SIGINT: exit immediately (non-zero code).
//! * A panic in a worker (here: `buffer_capacity` = `usize::MAX`, which
//!   `Buffer::try_new` rejects) prints a message naming the config's
//!   `csv_output_path` and the word `run`, keeps the runs completed so far
//!   checkpointed and exits non-zero. (Tests assume the bad config is only
//!   discovered when its job runs, as today.)
//! * Job order is run-major: run 0 of every config (config order), then run
//!   1 of every config that has one, and so on.
//! * Unknown flags exit non-zero.
//! * A completed sweep removes the checkpoint; CSV files are (re)written from
//!   the checkpoint rows once a config is complete; no temp files remain.

use lib::checkpoint::{self, Checkpoint, ConfigProgress, fingerprint};
use lib::config::Config;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use test_case::test_case;

// ------------------------------------------------------------------- harness

const TIMEOUT: Duration = Duration::from_secs(60);

/// A scratch working directory with `simulation_configs.json`; removed on drop.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(name: &str, configs_json: &str) -> Scratch {
        // unique per call: parameterised tests share `name` and run in parallel
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir()
            .join("dpds-simulation-tests")
            .join(format!(
                "cli-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("simulation_configs.json"), configs_json).unwrap();
        Scratch { dir }
    }

    fn results(&self) -> PathBuf {
        self.dir.join("results")
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.results().join(".checkpoint.json")
    }

    /// The binary with a clean environment (no worker count, no prompt or
    /// clock overrides), stdin and the rest to be set by the caller.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dpds_simulation"));
        command
            .current_dir(&self.dir)
            .args(args)
            .env_remove("MAX_CONCURRENT_SIMULATIONS")
            .env_remove("DPDS_SIM_FORCE_PROMPT")
            .env_remove("DPDS_SIM_FAKE_NOW");
        command
    }

    /// Every file below the scratch directory except the config, sorted.
    fn files(&self) -> Vec<String> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_string_lossy();
                    if rel != "simulation_configs.json" {
                        out.push(rel.into_owned());
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.dir, &self.dir, &mut out);
        out.sort();
        out
    }

    fn csv_files(&self) -> BTreeSet<String> {
        match fs::read_dir(self.results()) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".csv"))
                .collect(),
            Err(_) => BTreeSet::new(),
        }
    }

    fn read_checkpoint_bytes(&self) -> Vec<u8> {
        fs::read(self.checkpoint_path()).expect("checkpoint file")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A running binary with its combined stdout + stderr collected on the side.
struct Proc {
    child: Child,
    output: Arc<Mutex<Vec<u8>>>,
    /// stderr only (also part of `output`).
    errors: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
}

impl Proc {
    fn spawn(mut command: Command, stdin: Option<&str>) -> Proc {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        let mut child = command.spawn().expect("spawn dpds_simulation");
        if let Some(input) = stdin {
            let mut pipe = child.stdin.take().unwrap();
            // the child may exit before reading everything: not an error
            let _ = pipe.write_all(input.as_bytes());
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        fn pump(
            mut source: impl Read + Send + 'static,
            sinks: Vec<Arc<Mutex<Vec<u8>>>>,
        ) -> JoinHandle<()> {
            thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while let Ok(n) = source.read(&mut buffer) {
                    if n == 0 {
                        break;
                    }
                    for sink in &sinks {
                        sink.lock().unwrap().extend_from_slice(&buffer[..n]);
                    }
                }
            })
        }
        readers.push(pump(
            child.stdout.take().unwrap(),
            vec![Arc::clone(&output)],
        ));
        readers.push(pump(
            child.stderr.take().unwrap(),
            vec![Arc::clone(&output), Arc::clone(&errors)],
        ));
        Proc {
            child,
            output,
            errors,
            readers,
        }
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.errors.lock().unwrap()).into_owned()
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{name} failed");
    }

    fn try_status(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().expect("try_wait")
    }

    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_status() {
                for reader in self.readers.drain(..) {
                    let _ = reader.join();
                }
                return status;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!(
                    "timed out after {timeout:?}; output so far:\n{}",
                    self.output()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Outcome {
    status: ExitStatus,
    output: String,
}

impl Outcome {
    fn code(&self) -> Option<i32> {
        self.status.code()
    }
    /// Exited normally (not killed by a signal) with a non-zero code.
    fn failed_gracefully(&self) -> bool {
        self.code().is_some_and(|c| c != 0)
    }
}

fn run(scratch: &Scratch, args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>) -> Outcome {
    let mut command = scratch.command(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut proc = Proc::spawn(command, stdin);
    let status = proc.wait(TIMEOUT);
    Outcome {
        status,
        output: proc.output(),
    }
}

fn assert_success(outcome: &Outcome) {
    assert!(
        outcome.status.success(),
        "expected success, got {:?}:\n{}",
        outcome.status,
        outcome.output
    );
}

fn assert_mentions(outcome: &Outcome, needles: &[&str]) {
    for needle in needles {
        assert!(
            outcome.output.contains(needle),
            "output must mention {needle:?}:\n{}",
            outcome.output
        );
    }
}

// ------------------------------------------------------------------ fixtures

fn config_json(
    csv: &str,
    runs: u32,
    duration_s: f64,
    reorder: bool,
    seed: u64,
    extras: impl FnOnce(&mut Value),
) -> Value {
    let mut value = json!({
        "simulation_config": {
            "duration_s": duration_s, "runs": runs, "rate_gbps": 1.2,
            "average_frame_size_byte": 1480, "activate_reordering": reorder,
            "csv_output_path": csv, "seed": seed
        },
        "delay_config": {
            "mean_ns": 100000, "std_ns": 30000, "rv_distribution": "NormalDistribution",
            "activate_correlation": false, "half_life_period_ns": 0
        }
    });
    extras(&mut value);
    value
}

fn with_spacer(value: &mut Value) {
    value["limiter_config"] = json!({"mode": "Spacer", "rate_bps": 800000000});
}

fn with_loss(value: &mut Value) {
    value["simulation_config"]["loss_model"] = json!({"Random": 0.1});
}

fn no_extras(_: &mut Value) {}

fn to_text(values: &[Value]) -> String {
    serde_json::to_string_pretty(values).unwrap()
}

fn parse(text: &str) -> Vec<Config> {
    serde_json::from_str(text).expect("configs parse")
}

const SMALL_RUNS: u32 = 6;
const SMALL_FILES: [&str; 3] = ["a_fifo.csv", "b_reorder.csv", "c_spacer.csv"];

/// 3 configs x 6 runs, 41 packets per run (iat = 10 us): milliseconds in total.
fn small_values() -> Vec<Value> {
    vec![
        config_json("a_fifo.csv", SMALL_RUNS, 0.0004, false, 100, no_extras),
        config_json("b_reorder.csv", SMALL_RUNS, 0.0004, true, 200, no_extras),
        config_json("c_spacer.csv", SMALL_RUNS, 0.0004, false, 300, with_spacer),
    ]
}

fn small_text() -> String {
    to_text(&small_values())
}

/// Wall-clock seconds one simulated second of the heavy config (reordering
/// plus spacer, the costliest of the fixtures) costs in this build (debug or
/// release), measured in-process with the same library the binary uses. Jobs
/// whose duration matters are sized with it, so they take about the same time
/// in every profile.
///
/// Measured once per test process, over windows of at least 200 ms, and the
/// fastest of five samples (spaced out in time) counts: other tests run in
/// parallel and a load spike only ever makes a sample slower, so the minimum
/// can overestimate the cost only if the machine is loaded during all of them.
fn seconds_per_simulated_second() -> f64 {
    static COST: OnceLock<f64> = OnceLock::new();
    *COST.get_or_init(|| {
        let time = |duration_s: f64| {
            let config = &parse(&to_text(&[config_json(
                "calibration.csv",
                1,
                duration_s,
                true,
                1,
                with_spacer,
            )]))[0];
            let started = Instant::now();
            lib::simulation::run_simulation(config, 0);
            started.elapsed()
        };
        let mut duration_s = 0.05;
        let first = loop {
            let elapsed = time(duration_s);
            if elapsed >= Duration::from_millis(200) {
                break elapsed;
            }
            duration_s *= 4.0;
        };
        let fastest = (0..4)
            .map(|_| {
                thread::sleep(Duration::from_millis(50));
                time(duration_s)
            })
            .fold(first, Duration::min);
        fastest.as_secs_f64() / duration_s
    })
}

/// The `duration_s` that makes one run of the heavy config take about `target`.
fn duration_for(target: Duration) -> f64 {
    target.as_secs_f64() / seconds_per_simulated_second()
}

/// A `duration_s` no test outlives in any build: for runs that are never
/// meant to finish (the test signals or kills the process first). The memory
/// of a run does not grow with it.
const NEVER_ENDS_S: f64 = 100_000.0;

/// 4 configs x 12 runs + one with 2 runs, about 80 ms per run in any build.
fn slow_text() -> String {
    let duration_s = duration_for(Duration::from_millis(80)).max(0.01);
    to_text(&[
        config_json("s1.csv", 12, duration_s, false, 1, no_extras),
        config_json("s2.csv", 12, duration_s, true, 2, no_extras),
        config_json("s3.csv", 12, duration_s, true, 3, with_spacer),
        config_json("s4.csv", 12, duration_s, false, 4, with_loss),
        config_json("short.csv", 2, duration_s, true, 5, no_extras),
    ])
}

/// The genuine CSV rows (as strings, by run index) of every config, computed
/// with the library, one thread per config.
fn genuine_rows(configs: &[Config]) -> BTreeMap<String, Vec<Vec<String>>> {
    thread::scope(|scope| {
        let handles: Vec<_> = configs
            .iter()
            .map(|config| {
                scope.spawn(move || {
                    let rows: Vec<Vec<String>> = (0..config.simulation_config.runs)
                        .map(|run| {
                            lib::simulation::run_simulation(config, run)
                                .csv_record(run)
                                .to_vec()
                        })
                        .collect();
                    (config.simulation_config.csv_output_path.clone(), rows)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn slow_rows() -> &'static BTreeMap<String, Vec<Vec<String>>> {
    static ROWS: OnceLock<BTreeMap<String, Vec<Vec<String>>>> = OnceLock::new();
    ROWS.get_or_init(|| genuine_rows(&parse(&slow_text())))
}

const SENTINEL: &str = "7770000";

/// What a checkpoint stores for a run in the tests that plant rows: the
/// genuine row with a recognisable value in column 1. A resumed sweep that
/// re-ran the run would overwrite it with the genuine value.
fn sentinel_row(genuine: &[String]) -> Vec<String> {
    let mut row = genuine.to_vec();
    row[1] = SENTINEL.to_string();
    row
}

fn csv_text(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(header).unwrap();
    for row in rows {
        writer.write_record(row).unwrap();
    }
    String::from_utf8(writer.into_inner().unwrap()).unwrap()
}

/// Expected CSV text per config when the runs in `planted` come from the
/// checkpoint (sentinel rows) and all others were executed.
fn expected_csvs(
    rows: &BTreeMap<String, Vec<Vec<String>>>,
    planted: &[(&str, &[u32])],
) -> BTreeMap<String, String> {
    rows.iter()
        .map(|(csv, genuine)| {
            let planted_runs: BTreeSet<u32> = planted
                .iter()
                .filter(|(name, _)| name == csv)
                .flat_map(|(_, runs)| runs.iter().copied())
                .collect();
            let rows: Vec<Vec<String>> = genuine
                .iter()
                .enumerate()
                .map(|(run, row)| {
                    if planted_runs.contains(&(run as u32)) {
                        sentinel_row(row)
                    } else {
                        row.clone()
                    }
                })
                .collect();
            (csv.clone(), csv_text(&lib::results::CSV_HEADER, &rows))
        })
        .collect()
}

/// Asserts that `results/` holds exactly these CSVs with exactly this text
/// and nothing else (no checkpoint, no temp files).
fn assert_results_are(scratch: &Scratch, expected: &BTreeMap<String, String>) {
    for (csv, text) in expected {
        let actual = fs::read_to_string(scratch.results().join(csv))
            .unwrap_or_else(|e| panic!("{csv} missing: {e}"));
        assert_eq!(&actual, text, "{csv}");
    }
    let mut files: Vec<String> = expected
        .keys()
        .map(|csv| format!("results/{csv}"))
        .collect();
    files.sort();
    assert_eq!(scratch.files(), files, "results/ must hold only the CSVs");
}

/// Writes `results/.checkpoint.json` for `configs` with sentinel rows for the
/// given completed runs; returns its bytes.
fn plant_checkpoint(
    scratch: &Scratch,
    configs: &[Config],
    genuine: &BTreeMap<String, Vec<Vec<String>>>,
    completed: &[(&str, &[u32])],
) -> Vec<u8> {
    fs::create_dir_all(scratch.results()).unwrap();
    let mut cp = Checkpoint::new(configs);
    for (csv, runs) in completed {
        for &run in *runs {
            cp.record(csv, run, sentinel_row(&genuine[*csv][run as usize]));
        }
    }
    checkpoint::save(&scratch.checkpoint_path(), &cp).unwrap();
    scratch.read_checkpoint_bytes()
}

/// A partial checkpoint over the small configs: 4 of 18 runs, one config
/// with a non-contiguous set.
const PLANTED: [(&str, &[u32]); 2] = [("a_fifo.csv", &[0, 2, 5]), ("b_reorder.csv", &[1])];

struct Small {
    scratch: Scratch,
    configs: Vec<Config>,
    genuine: BTreeMap<String, Vec<Vec<String>>>,
}

impl Small {
    fn new(name: &str) -> Small {
        let text = small_text();
        let configs = parse(&text);
        let genuine = genuine_rows(&configs);
        Small {
            scratch: Scratch::new(name, &text),
            configs,
            genuine,
        }
    }

    fn with_planted(name: &str) -> (Small, Vec<u8>) {
        let small = Small::new(name);
        let bytes = plant_checkpoint(&small.scratch, &small.configs, &small.genuine, &PLANTED);
        (small, bytes)
    }

    fn expect_resumed(&self) -> BTreeMap<String, String> {
        expected_csvs(&self.genuine, &PLANTED)
    }

    fn expect_fresh(&self) -> BTreeMap<String, String> {
        expected_csvs(&self.genuine, &[])
    }
}

fn total_rows(cp: &Checkpoint) -> usize {
    cp.configs.values().map(|c| c.rows.len()).sum()
}

/// Run-major job list, computed from the run counts only.
fn run_major_plan(configs: &[Config]) -> Vec<(String, u32)> {
    let max_runs = configs
        .iter()
        .map(|c| c.simulation_config.runs)
        .max()
        .unwrap_or(0);
    (0..max_runs)
        .flat_map(|run| {
            configs
                .iter()
                .filter(move |c| c.simulation_config.runs > run)
                .map(move |c| (c.simulation_config.csv_output_path.clone(), run))
        })
        .collect()
}

/// Polls the checkpoint of a running sweep until `done` holds. Every
/// successfully read file must be valid (atomic writes).
fn wait_for_checkpoint(
    scratch: &Scratch,
    proc: &mut Proc,
    done: impl Fn(&Checkpoint) -> bool,
) -> Checkpoint {
    let path = scratch.checkpoint_path();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(status) = proc.try_status() {
            panic!(
                "sweep ended ({status:?}) before the expected state; make the jobs slower:\n{}",
                proc.output()
            );
        }
        if path.exists() {
            match checkpoint::load(&path) {
                Ok(cp) if done(&cp) => return cp,
                Ok(_) => {}
                // finished in between: the file is gone, the next loop sees the exit
                Err(_) if !path.exists() => {}
                Err(e) => panic!("a checkpoint visible while running must be valid: {e}"),
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the checkpoint"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

fn assert_recorded_rows_are_genuine(cp: &Checkpoint, genuine: &BTreeMap<String, Vec<Vec<String>>>) {
    for (csv, progress) in &cp.configs {
        for (run, row) in &progress.rows {
            assert_eq!(row, &genuine[csv][*run as usize], "{csv} run {run}");
        }
    }
}

// ------------------------------------------------- plain sweep, per-run saves

#[test]
fn uninterrupted_sweep_writes_the_expected_csvs_and_removes_the_checkpoint() {
    let small = Small::new("plain");
    let outcome = run(
        &small.scratch,
        &[],
        &[("MAX_CONCURRENT_SIMULATIONS", "3")],
        None,
    );
    assert_success(&outcome);
    assert_results_are(&small.scratch, &small.expect_fresh());
}

#[test]
fn checkpoint_is_saved_after_every_run_not_only_on_exit() {
    // SIGKILL cannot run any handler: whatever is on disk was saved by the
    // per-run saves. Also checks run-major order (one worker => completion
    // order = job order) and the stable cross-process fingerprint.
    let text = slow_text();
    let configs = parse(&text);
    let scratch = Scratch::new("sigkill", &text);
    let mut proc = Proc::spawn(
        {
            let mut c = scratch.command(&[]);
            c.env("MAX_CONCURRENT_SIMULATIONS", "1");
            c
        },
        None,
    );
    // the checkpoint appears right after the first completed runs, not later
    let first = wait_for_checkpoint(&scratch, &mut proc, |_| true);
    assert!(
        total_rows(&first) <= 6,
        "first checkpoint already holds {} runs: saved too rarely",
        total_rows(&first)
    );
    wait_for_checkpoint(&scratch, &mut proc, |cp| total_rows(cp) >= 3);
    proc.signal("KILL");
    let status = proc.wait(TIMEOUT);
    assert!(status.code().is_none(), "must have been killed: {status:?}");

    let cp = checkpoint::load(&scratch.checkpoint_path())
        .expect("a SIGKILL must leave a valid checkpoint (atomic writes)");
    assert_eq!(cp.format, "dpds-simulation-checkpoint");
    assert_eq!(cp.version, 1);
    assert_eq!(
        cp.fingerprint,
        fingerprint(&configs),
        "fingerprint written by the binary == library fingerprint"
    );
    let plan = run_major_plan(&configs);
    let done = total_rows(&cp);
    assert!(
        (3..plan.len()).contains(&done),
        "{done} of {} jobs done",
        plan.len()
    );
    // exactly the first `done` jobs of the run-major plan are recorded
    let mut expected: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for (csv, run) in &plan[..done] {
        expected.entry(csv.clone()).or_default().insert(*run);
    }
    for (csv, progress) in &cp.configs {
        let recorded: BTreeSet<u32> = progress.rows.keys().copied().collect();
        assert_eq!(
            &recorded,
            expected.get(csv).unwrap_or(&BTreeSet::new()),
            "{csv}: not the run-major prefix of {done} jobs"
        );
    }
    assert_recorded_rows_are_genuine(&cp, slow_rows());
    // a CSV may only exist for a config that is complete in the checkpoint
    for csv in scratch.csv_files() {
        assert_eq!(
            cp.configs[&csv].rows.len() as u32,
            cp.configs[&csv].runs,
            "{csv}"
        );
    }

    // resume with another worker count: identical to the independent oracle
    let outcome = run(
        &scratch,
        &["--resume"],
        &[("MAX_CONCURRENT_SIMULATIONS", "4")],
        None,
    );
    assert_success(&outcome);
    assert_results_are(&scratch, &expected_csvs(slow_rows(), &[]));
}

// ---------------------------------------------------------------- resume logic

#[test]
fn resume_skips_exactly_the_recorded_indices() {
    // recorded: a {0,2,5} (non-contiguous), b {1}, c {}: sentinel rows must
    // survive in the final CSVs, every other run must have been executed.
    let (small, _) = Small::with_planted("resume-exact");
    let outcome = run(&small.scratch, &["--resume"], &[], None);
    assert_success(&outcome);
    assert_results_are(&small.scratch, &small.expect_resumed());
}

#[test]
fn resume_with_other_non_contiguous_sets() {
    let small = Small::new("resume-sets");
    let planted: [(&str, &[u32]); 3] = [
        ("a_fifo.csv", &[1, 3]),
        ("b_reorder.csv", &[0, 4, 5]),
        ("c_spacer.csv", &[2]),
    ];
    plant_checkpoint(&small.scratch, &small.configs, &small.genuine, &planted);
    let outcome = run(
        &small.scratch,
        &["--resume"],
        &[("MAX_CONCURRENT_SIMULATIONS", "2")],
        None,
    );
    assert_success(&outcome);
    assert_results_are(&small.scratch, &expected_csvs(&small.genuine, &planted));
}

#[test]
fn resume_writes_the_csv_of_a_config_that_is_complete_in_the_checkpoint() {
    // a_fifo complete, others empty. A stale CSV of a_fifo must be rewritten
    // from the checkpoint rows (sorted by run).
    let small = Small::new("resume-complete-config");
    let planted: [(&str, &[u32]); 1] = [("a_fifo.csv", &[0, 1, 2, 3, 4, 5])];
    plant_checkpoint(&small.scratch, &small.configs, &small.genuine, &planted);
    fs::write(small.scratch.results().join("a_fifo.csv"), "stale\n").unwrap();
    let outcome = run(&small.scratch, &["--resume"], &[], None);
    assert_success(&outcome);
    assert_results_are(&small.scratch, &expected_csvs(&small.genuine, &planted));
}

#[test]
fn resume_of_an_already_complete_checkpoint_runs_nothing() {
    // Every run recorded (a crash right before the final cleanup): the CSVs
    // are rebuilt from the sentinel rows and the checkpoint disappears.
    let small = Small::new("resume-all-done");
    let all: Vec<(&str, Vec<u32>)> = SMALL_FILES
        .iter()
        .map(|f| (*f, (0..SMALL_RUNS).collect()))
        .collect();
    let planted: Vec<(&str, &[u32])> = all.iter().map(|(f, r)| (*f, r.as_slice())).collect();
    plant_checkpoint(&small.scratch, &small.configs, &small.genuine, &planted);
    let outcome = run(&small.scratch, &["--resume"], &[], None);
    assert_success(&outcome);
    let expected = expected_csvs(&small.genuine, &planted);
    assert!(
        expected
            .values()
            .all(|text| text.matches(SENTINEL).count() == SMALL_RUNS as usize),
        "oracle: every row is a sentinel"
    );
    assert_results_are(&small.scratch, &expected);
}

#[test]
fn resume_summary_shows_completed_over_total_runs() {
    let (small, _) = Small::with_planted("resume-summary");
    let outcome = run(&small.scratch, &["--resume"], &[], None);
    assert_success(&outcome);
    assert_mentions(&outcome, &["4/18"]);
}

#[test]
fn resume_accepts_reformatted_configs() {
    // The fingerprint is of the parsed content: other whitespace and key
    // order (serde_json::Value sorts the keys) must still match.
    let (small, _) = Small::with_planted("resume-reformat");
    let reparsed: Value = serde_json::from_str(&small_text()).unwrap();
    fs::write(
        small.scratch.dir.join("simulation_configs.json"),
        serde_json::to_string(&reparsed).unwrap(),
    )
    .unwrap();
    let outcome = run(&small.scratch, &["--resume"], &[], None);
    assert_success(&outcome);
    assert_results_are(&small.scratch, &small.expect_resumed());
}

// ------------------------------------------------------ --fresh / flags / TTY

#[test]
fn fresh_deletes_the_checkpoint_and_restarts_without_asking() {
    let (small, _) = Small::with_planted("fresh");
    // forced prompt + empty stdin: a prompt would fail, the flag must win
    let outcome = run(
        &small.scratch,
        &["--fresh"],
        &[("DPDS_SIM_FORCE_PROMPT", "1")],
        Some(""),
    );
    assert_success(&outcome);
    assert!(
        !outcome.output.contains("Really delete"),
        "{}",
        outcome.output
    );
    assert!(!outcome.output.contains("Resume it?"), "{}", outcome.output);
    assert_results_are(&small.scratch, &small.expect_fresh());
}

#[test]
fn resume_flag_does_not_prompt() {
    let (small, _) = Small::with_planted("resume-flag");
    let outcome = run(
        &small.scratch,
        &["--resume"],
        &[("DPDS_SIM_FORCE_PROMPT", "1")],
        Some(""),
    );
    assert_success(&outcome);
    assert!(!outcome.output.contains("Resume it?"), "{}", outcome.output);
    assert_results_are(&small.scratch, &small.expect_resumed());
}

#[test]
fn resume_and_fresh_are_mutually_exclusive() {
    // with a checkpoint
    let (small, bytes) = Small::with_planted("exclusive");
    for args in [["--resume", "--fresh"], ["--fresh", "--resume"]] {
        let outcome = run(&small.scratch, &args, &[], None);
        assert!(outcome.failed_gracefully(), "{args:?}: {}", outcome.output);
        assert_mentions(&outcome, &["--resume", "--fresh"]);
        assert_eq!(small.scratch.read_checkpoint_bytes(), bytes);
        assert!(small.scratch.csv_files().is_empty());
    }
    // and without one: nothing is written either
    let bare = Scratch::new("exclusive-bare", &small_text());
    let outcome = run(&bare, &["--resume", "--fresh"], &[], None);
    assert!(outcome.failed_gracefully());
    assert_mentions(&outcome, &["--resume", "--fresh"]);
    assert!(bare.files().is_empty(), "{:?}", bare.files());
}

#[test]
fn without_a_tty_and_without_a_flag_an_existing_checkpoint_is_an_error() {
    let (small, bytes) = Small::with_planted("no-tty");
    // stdin is /dev/null here; the stdin of a test run must not matter either
    for stdin in [None, Some("y\n")] {
        let outcome = run(&small.scratch, &[], &[], stdin);
        assert!(outcome.failed_gracefully(), "{stdin:?}: {}", outcome.output);
        assert_mentions(&outcome, &["--resume", "--fresh"]);
        assert_eq!(small.scratch.read_checkpoint_bytes(), bytes);
        assert!(small.scratch.csv_files().is_empty());
    }
}

#[test]
fn unknown_flags_are_rejected() {
    let small = Small::new("unknown-flag");
    let outcome = run(&small.scratch, &["--bogus"], &[], None);
    assert!(outcome.failed_gracefully(), "{}", outcome.output);
    assert!(
        small.scratch.files().is_empty(),
        "{:?}",
        small.scratch.files()
    );
}

// ---------------------------------------------------------- interactive prompt

const PROMPT_ENV: [(&str, &str); 1] = [("DPDS_SIM_FORCE_PROMPT", "1")];

#[test]
fn prompt_yes_resumes() {
    let (small, _) = Small::with_planted("prompt-y");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some("y\n"));
    assert_success(&outcome);
    assert_mentions(&outcome, &["4/18", "Resume it? [y/n]"]);
    assert!(
        !outcome.output.contains("Really delete"),
        "{}",
        outcome.output
    );
    assert_results_are(&small.scratch, &small.expect_resumed());
}

#[test]
fn prompt_no_then_yes_deletes_the_checkpoint_and_starts_fresh() {
    let (small, _) = Small::with_planted("prompt-ny");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some("n\ny\n"));
    assert_success(&outcome);
    assert_mentions(
        &outcome,
        &[
            "Resume it? [y/n]",
            "Really delete the checkpoint and start fresh? [y/n]",
        ],
    );
    assert_results_are(&small.scratch, &small.expect_fresh());
}

#[test]
fn prompt_no_then_no_exits_and_leaves_the_checkpoint_untouched() {
    let (small, bytes) = Small::with_planted("prompt-nn");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some("n\nn\n"));
    assert!(outcome.failed_gracefully(), "{}", outcome.output);
    assert_mentions(
        &outcome,
        &["Really delete the checkpoint and start fresh? [y/n]"],
    );
    assert_eq!(small.scratch.read_checkpoint_bytes(), bytes);
    assert!(small.scratch.csv_files().is_empty());
}

#[test]
fn prompts_ask_again_on_invalid_input() {
    // first prompt: two invalid answers, then y
    let (small, _) = Small::with_planted("prompt-invalid-1");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some("maybe\n\nx\ny\n"));
    assert_success(&outcome);
    assert!(
        outcome.output.matches("Resume it? [y/n]").count() >= 3,
        "must ask again after each invalid answer:\n{}",
        outcome.output
    );
    assert_results_are(&small.scratch, &small.expect_resumed());

    // second prompt: invalid answer, then y
    let (small, _) = Small::with_planted("prompt-invalid-2");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some("n\nwhat\ny\n"));
    assert_success(&outcome);
    assert!(
        outcome
            .output
            .matches("Really delete the checkpoint and start fresh? [y/n]")
            .count()
            >= 2,
        "{}",
        outcome.output
    );
    assert_results_are(&small.scratch, &small.expect_fresh());
}

#[test_case(""; "no answer at all")]
#[test_case("n\n"; "eof at the second prompt")]
#[test_case("maybe\n"; "eof after an invalid answer")]
fn eof_on_stdin_is_an_error_and_never_a_guess(input: &str) {
    let (small, bytes) = Small::with_planted("prompt-eof");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some(input));
    assert!(outcome.failed_gracefully(), "{}", outcome.output);
    assert_eq!(small.scratch.read_checkpoint_bytes(), bytes);
    assert!(small.scratch.csv_files().is_empty());
}

#[test]
fn no_checkpoint_means_no_prompt() {
    let small = Small::new("prompt-none");
    let outcome = run(&small.scratch, &[], &PROMPT_ENV, Some(""));
    assert_success(&outcome);
    assert!(!outcome.output.contains("[y/n]"), "{}", outcome.output);
    assert_results_are(&small.scratch, &small.expect_fresh());
}

// ---------------------------------------------- abort on a checkpoint we can't use

/// Runs `args` against a bad checkpoint: must abort, name the file and
/// `--fresh`, leave the bytes alone and write no CSV.
fn assert_aborts(small: &Small, bytes: &[u8], args: &[&str], what: &str) {
    for stdin in [None, Some("y\n")] {
        let env: &[(&str, &str)] = if stdin.is_some() { &PROMPT_ENV } else { &[] };
        let outcome = run(&small.scratch, args, env, stdin);
        assert!(outcome.failed_gracefully(), "{what}: {}", outcome.output);
        assert!(
            !outcome.output.contains("panicked"),
            "{what}: an unusable checkpoint is an error message, not a panic:\n{}",
            outcome.output
        );
        assert_mentions(&outcome, &[".checkpoint.json", "--fresh"]);
        assert_eq!(
            small.scratch.read_checkpoint_bytes(),
            bytes,
            "{what}: checkpoint modified"
        );
        assert!(small.scratch.csv_files().is_empty(), "{what}: ran anyway");
    }
}

#[test]
fn fingerprint_mismatch_aborts_instead_of_starting_fresh() {
    let (small, bytes) = Small::with_planted("mismatch");
    let mut values = small_values();
    values[1]["simulation_config"]["seed"] = json!(201); // changes the results
    fs::write(
        small.scratch.dir.join("simulation_configs.json"),
        to_text(&values),
    )
    .unwrap();
    assert_aborts(&small, &bytes, &["--resume"], "changed seed, --resume");
    assert_aborts(&small, &bytes, &[], "changed seed, prompt/no flag");

    // a removed config and a renamed output file are mismatches as well
    for (what, values) in [
        ("config removed", small_values()[..2].to_vec()),
        ("csv renamed", {
            let mut v = small_values();
            v[2]["simulation_config"]["csv_output_path"] = json!("renamed.csv");
            v
        }),
        ("rate changed", {
            let mut v = small_values();
            v[0]["simulation_config"]["rate_gbps"] = json!(1.3);
            v
        }),
    ] {
        fs::write(
            small.scratch.dir.join("simulation_configs.json"),
            to_text(&values),
        )
        .unwrap();
        assert_aborts(&small, &bytes, &["--resume"], what);
    }
}

#[test]
fn fresh_is_the_way_out_of_a_fingerprint_mismatch() {
    let (small, _) = Small::with_planted("mismatch-fresh");
    let mut values = small_values();
    values[1]["simulation_config"]["seed"] = json!(201);
    let text = to_text(&values);
    fs::write(small.scratch.dir.join("simulation_configs.json"), &text).unwrap();
    let outcome = run(&small.scratch, &["--fresh"], &[], None);
    assert_success(&outcome);
    let configs = parse(&text);
    assert_results_are(&small.scratch, &expected_csvs(&genuine_rows(&configs), &[]));
}

#[test]
fn corrupt_checkpoints_abort_for_every_kind_of_damage() {
    let (small, good) = Small::with_planted("corrupt");
    let valid: Value = serde_json::from_slice(&good).unwrap();
    let mutated = |edit: &dyn Fn(&mut Value)| {
        let mut v = valid.clone();
        edit(&mut v);
        serde_json::to_vec_pretty(&v).unwrap()
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("garbage", b"definitely not json".to_vec()),
        ("empty file", Vec::new()),
        ("truncated", good[..good.len() / 2].to_vec()),
        ("wrong version", mutated(&|v| v["version"] = json!(2))),
        ("version zero", mutated(&|v| v["version"] = json!(0))),
        (
            "other format",
            mutated(&|v| v["format"] = json!("some-other-tool")),
        ),
        (
            "run index out of range",
            mutated(&|v| {
                v["configs"]["a_fifo.csv"]["rows"]["6"] =
                    json!(vec!["0"; lib::results::CSV_HEADER.len()]);
            }),
        ),
        (
            "row of the wrong length",
            mutated(&|v| {
                v["configs"]["a_fifo.csv"]["rows"]["0"] = json!(["0", "1"]);
            }),
        ),
    ];
    for (what, bytes) in cases {
        fs::write(small.scratch.checkpoint_path(), &bytes).unwrap();
        assert_aborts(&small, &bytes, &["--resume"], what);
    }
}

#[test]
fn fresh_also_replaces_an_unreadable_checkpoint() {
    let small = Small::new("corrupt-fresh");
    fs::create_dir_all(small.scratch.results()).unwrap();
    fs::write(small.scratch.checkpoint_path(), b"garbage").unwrap();
    let outcome = run(&small.scratch, &["--fresh"], &[], None);
    assert_success(&outcome);
    assert_results_are(&small.scratch, &small.expect_fresh());
}

/// Saves a checkpoint of the small configs with the planted sentinel rows,
/// changed by `edit` (the fingerprint stays the one of the current configs),
/// and returns its bytes.
fn plant_edited_checkpoint(small: &Small, edit: impl FnOnce(&mut Checkpoint)) -> Vec<u8> {
    fs::create_dir_all(small.scratch.results()).unwrap();
    let mut cp = Checkpoint::new(&small.configs);
    for (csv, runs) in PLANTED {
        for &run in runs {
            cp.record(csv, run, sentinel_row(&small.genuine[csv][run as usize]));
        }
    }
    edit(&mut cp);
    assert_eq!(cp.fingerprint, fingerprint(&small.configs));
    checkpoint::save(&small.scratch.checkpoint_path(), &cp).unwrap();
    small.scratch.read_checkpoint_bytes()
}

#[test]
fn a_checkpoint_with_other_config_keys_than_the_configs_aborts_cleanly() {
    // The fingerprint matches, but the file lacks a config, has an unknown
    // one, or both (hand-edited): resuming it would index a missing entry or
    // count runs of a config that does not exist.
    let small = Small::new("keys-mismatch");
    type Edit = fn(&mut Checkpoint);
    let cases: [(&str, Edit); 3] = [
        ("key missing", |cp| {
            cp.configs.remove("c_spacer.csv");
        }),
        ("key extra", |cp| {
            let extra = ConfigProgress {
                runs: 3,
                rows: BTreeMap::new(),
            };
            cp.configs.insert("extra.csv".to_string(), extra);
        }),
        ("key renamed", |cp| {
            let moved = cp.configs.remove("c_spacer.csv").unwrap();
            cp.configs.insert("renamed.csv".to_string(), moved);
        }),
    ];
    for (what, edit) in cases {
        let bytes = plant_edited_checkpoint(&small, edit);
        assert_aborts(&small, &bytes, &["--resume"], what);
    }
}

#[test]
fn a_checkpoint_with_another_run_count_than_the_config_aborts_cleanly() {
    let small = Small::new("runs-mismatch");
    for runs in [4, 9] {
        let bytes = plant_edited_checkpoint(&small, |cp| {
            cp.configs.get_mut("b_reorder.csv").unwrap().runs = runs;
        });
        assert_aborts(
            &small,
            &bytes,
            &["--resume"],
            &format!("b_reorder.csv with {runs} runs"),
        );
    }
}

#[test]
fn a_row_whose_first_field_is_not_its_run_index_aborts_cleanly() {
    // the row of run 2 stored under index 0, a row that says run 7, and a
    // first field that is not a number at all
    let small = Small::new("row-index-mismatch");
    type Edit = fn(&mut Checkpoint, &Small);
    let cases: [(&str, Edit); 3] = [
        ("run 2's row under index 0", |cp, small| {
            let row = small.genuine["a_fifo.csv"][2].clone();
            cp.configs
                .get_mut("a_fifo.csv")
                .unwrap()
                .rows
                .insert(0, row);
        }),
        ("first field is another run", |cp, small| {
            let mut row = small.genuine["b_reorder.csv"][1].clone();
            row[0] = "7".to_string();
            cp.configs
                .get_mut("b_reorder.csv")
                .unwrap()
                .rows
                .insert(1, row);
        }),
        ("first field is not a number", |cp, small| {
            let mut row = small.genuine["a_fifo.csv"][5].clone();
            row[0] = "five".to_string();
            cp.configs
                .get_mut("a_fifo.csv")
                .unwrap()
                .rows
                .insert(5, row);
        }),
    ];
    for (what, edit) in cases {
        let bytes = plant_edited_checkpoint(&small, |cp| edit(cp, &small));
        assert_aborts(&small, &bytes, &["--resume"], what);
    }
}

// --------------------------------------------------------------------- dry run

fn expected_estimate_and_jobs() -> (u64, u32) {
    // 3 configs x 6 runs x (floor(400000 / 10000) + 1 = 41) arrivals
    (3 * 6 * 41, 18)
}

#[test]
fn dry_run_prints_the_plan_and_writes_nothing() {
    let small = Small::new("dry");
    let (packets, jobs) = expected_estimate_and_jobs();
    assert_eq!(packets, 738);
    let outcome = run(&small.scratch, &["--dry-run"], &[], None);
    assert_success(&outcome);
    assert_mentions(&outcome, &SMALL_FILES);
    assert_mentions(&outcome, &[&packets.to_string(), &jobs.to_string()]);
    assert!(
        small.scratch.files().is_empty(),
        "nothing may be written: {:?}",
        small.scratch.files()
    );
}

#[test]
fn dry_run_with_a_checkpoint_shows_the_resume_state_and_leaves_it_alone() {
    let (small, bytes) = Small::with_planted("dry-resume");
    // no flag, no TTY, and even a forced prompt with empty stdin: no prompt
    for env in [&[][..], &PROMPT_ENV[..]] {
        let outcome = run(&small.scratch, &["--dry-run"], env, Some(""));
        assert_success(&outcome);
        assert_mentions(&outcome, &SMALL_FILES);
        assert_mentions(&outcome, &["4/18"]);
        assert!(!outcome.output.contains("[y/n]"), "{}", outcome.output);
        assert_eq!(small.scratch.read_checkpoint_bytes(), bytes);
        assert!(small.scratch.csv_files().is_empty());
        assert_eq!(small.scratch.files(), ["results/.checkpoint.json"]);
    }
}

#[test]
fn dry_run_does_not_wait_for_start_at() {
    let small = Small::new("dry-start-at");
    let started = Instant::now();
    let outcome = run(
        &small.scratch,
        &["--dry-run", "--start-at", "09:59"],
        &[("DPDS_SIM_FAKE_NOW", "10:00:00")],
        None,
    );
    assert_success(&outcome);
    assert!(started.elapsed() < Duration::from_secs(20));
    assert!(small.scratch.files().is_empty());
}

// ------------------------------------------------------------- time arguments

#[test_case("--stop-at", "25:00"; "stop hour too large")]
#[test_case("--stop-at", "12:60"; "stop minute too large")]
#[test_case("--stop-at", "abc"; "stop not a time")]
#[test_case("--stop-at", "12"; "stop without minutes")]
#[test_case("--stop-at", "12:00:00"; "stop with seconds")]
#[test_case("--stop-at", "12:00pm"; "stop with suffix")]
#[test_case("--stop-at", "-1:00"; "stop negative")]
#[test_case("--stop-at", ""; "stop empty")]
#[test_case("--start-at", "24:00"; "start hour too large")]
#[test_case("--start-at", "7:75"; "start minute too large")]
#[test_case("--start-at", "noon"; "start not a time")]
#[test_case("--start-at", "1:2:3"; "start with seconds")]
fn invalid_times_are_rejected_before_anything_is_written(flag: &str, value: &str) {
    let small = Small::new("bad-time");
    let outcome = run(&small.scratch, &[flag, value], &[], None);
    assert!(outcome.failed_gracefully(), "{}", outcome.output);
    assert_mentions(&outcome, &[flag]);
    assert!(
        small.scratch.files().is_empty(),
        "{:?}",
        small.scratch.files()
    );
}

#[test_case("--stop-at"; "stop")]
#[test_case("--start-at"; "start")]
fn a_time_flag_without_a_value_is_rejected(flag: &str) {
    let small = Small::new("missing-time");
    let outcome = run(&small.scratch, &[flag], &[], None);
    assert!(outcome.failed_gracefully(), "{}", outcome.output);
    assert!(small.scratch.files().is_empty());
}

#[test]
fn valid_times_are_accepted() {
    let small = Small::new("good-time");
    for args in [
        vec!["--dry-run", "--stop-at", "02:00"],
        vec!["--dry-run", "--stop-at", "2:00"],
        vec!["--dry-run", "--start-at", "23:59"],
        vec!["--dry-run", "--start-at", "0:0", "--stop-at", "23:59"],
    ] {
        let outcome = run(&small.scratch, &args, &[], None);
        assert_success(&outcome);
    }
}

#[test]
fn a_stop_time_that_already_passed_means_tomorrow_and_does_not_stop_the_sweep() {
    let small = Small::new("stop-past");
    for stop in ["09:59", "10:00"] {
        let outcome = run(
            &small.scratch,
            &["--stop-at", stop],
            &[("DPDS_SIM_FAKE_NOW", "10:00:00")],
            None,
        );
        assert_success(&outcome);
        assert_results_are(&small.scratch, &small.expect_fresh());
    }
}

/// What a stop caused by `--stop-at` must say (any case): that the deadline
/// was reached, or which flag it was.
fn mentions_the_stop_at_deadline(output: &str) -> bool {
    let output = output.to_lowercase();
    output.contains("deadline") || output.contains("--stop-at")
}

#[test]
fn stop_at_in_the_near_future_stops_gracefully_and_resume_completes() {
    // 3 configs x 20 runs of ~150 ms (in any build) on one worker: about 7 s
    // of work, of which the stop at +2 s lets only a part run.
    let duration_s = duration_for(Duration::from_millis(150));
    let text = to_text(&[
        config_json("t1.csv", 20, duration_s, false, 11, no_extras),
        config_json("t2.csv", 20, duration_s, true, 12, no_extras),
        config_json("t3.csv", 20, duration_s, true, 13, with_spacer),
    ]);
    let configs = parse(&text);
    let scratch = Scratch::new("stop-soon", &text);
    let started = Instant::now();
    let outcome = run(
        &scratch,
        &["--stop-at", "11:00"],
        &[
            ("DPDS_SIM_FAKE_NOW", "10:59:58"),
            ("MAX_CONCURRENT_SIMULATIONS", "1"),
        ],
        None,
    );
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "stopped too early"
    );
    assert!(outcome.failed_gracefully(), "{}", outcome.output);
    assert_mentions(&outcome, &["--resume"]);
    assert!(
        mentions_the_stop_at_deadline(&outcome.output),
        "the output must say that the --stop-at deadline was reached:\n{}",
        outcome.output
    );

    let cp = checkpoint::load(&scratch.checkpoint_path()).expect("checkpoint after the stop");
    assert_eq!(cp.fingerprint, fingerprint(&configs));
    let done = total_rows(&cp);
    let jobs = run_major_plan(&configs).len();
    assert!((1..jobs).contains(&done), "{done} of {jobs} jobs done");
    let genuine = genuine_rows(&configs);
    assert_recorded_rows_are_genuine(&cp, &genuine);
    assert!(
        scratch.csv_files().is_empty(),
        "no config can be complete yet"
    );

    let outcome = run(
        &scratch,
        &["--resume"],
        &[("MAX_CONCURRENT_SIMULATIONS", "6")],
        None,
    );
    assert_success(&outcome);
    assert_results_are(&scratch, &expected_csvs(&genuine, &[]));
}

#[test]
fn stop_at_is_counted_from_the_start_moment_not_from_the_launch() {
    // Launched at 10:59:58, `--start-at 11:00` waits 2 s. `--stop-at 11:00`
    // is the same time of day as the start moment, i.e. (as for a stop time
    // that equals "now") tomorrow: the sweep must run completely. A deadline
    // counted from the launch (2 s away) would already be over when the wait
    // ends, and the sweep would stop at once with 0 runs.
    //
    // (The case from the review, 03:00 launch, `--start-at 02:00 --stop-at
    // 06:00`, needs a 23 h wait and cannot be run with this seam.)
    let small = Small::new("stop-after-start");
    let outcome = run(
        &small.scratch,
        &["--start-at", "11:00", "--stop-at", "11:00"],
        &[("DPDS_SIM_FAKE_NOW", "10:59:58")],
        None,
    );
    assert_success(&outcome);
    assert_results_are(&small.scratch, &small.expect_fresh());
}

#[test]
fn start_at_waits_until_that_time() {
    let small = Small::new("start-wait");
    let started = Instant::now();
    let outcome = run(
        &small.scratch,
        &["--start-at", "11:00"],
        &[("DPDS_SIM_FAKE_NOW", "10:59:58")],
        None,
    );
    assert_success(&outcome);
    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "started after only {:?}",
        started.elapsed()
    );
    assert_results_are(&small.scratch, &small.expect_fresh());
}

#[test]
fn a_start_time_that_already_passed_means_tomorrow_and_nothing_runs_meanwhile() {
    let small = Small::new("start-tomorrow");
    let mut proc = Proc::spawn(
        {
            let mut c = small.scratch.command(&["--start-at", "09:59"]);
            c.env("DPDS_SIM_FAKE_NOW", "10:00:00");
            c
        },
        None,
    );
    wait_for_output(&mut proc, "until --start-at");
    thread::sleep(Duration::from_millis(500));
    assert!(
        proc.try_status().is_none(),
        "must still be waiting:\n{}",
        proc.output()
    );
    assert!(small.scratch.csv_files().is_empty());
    assert!(
        small.scratch.files().is_empty(),
        "{:?}",
        small.scratch.files()
    );
}

#[test]
fn prompts_come_before_the_start_at_wait_and_nothing_is_touched_while_waiting() {
    let (small, bytes) = Small::with_planted("start-after-prompt");
    let mut proc = Proc::spawn(
        {
            let mut c = small.scratch.command(&["--start-at", "09:59"]);
            c.env("DPDS_SIM_FAKE_NOW", "10:00:00");
            c.env("DPDS_SIM_FORCE_PROMPT", "1");
            c
        },
        Some("y\n"),
    );
    wait_for_output(&mut proc, "until --start-at");
    thread::sleep(Duration::from_millis(500));
    assert!(
        proc.try_status().is_none(),
        "must be waiting:\n{}",
        proc.output()
    );
    assert!(
        proc.output().contains("Resume it? [y/n]"),
        "the prompt must be shown before the wait:\n{}",
        proc.output()
    );
    assert_eq!(small.scratch.read_checkpoint_bytes(), bytes);
    assert!(small.scratch.csv_files().is_empty());
}

// ------------------------------------------------------------------- signals

#[test_case("INT"; "SIGINT")]
#[test_case("TERM"; "SIGTERM")]
fn first_signal_stops_gracefully_and_resume_completes_to_identical_csvs(signal: &str) {
    let text = slow_text();
    let configs = parse(&text);
    let scratch = Scratch::new(&format!("signal-{signal}"), &text);
    let mut proc = Proc::spawn(
        {
            let mut c = scratch.command(&[]);
            c.env("MAX_CONCURRENT_SIMULATIONS", "2");
            c
        },
        None,
    );
    // run-major: the 2-run config is complete after the first 10 jobs
    wait_for_checkpoint(&scratch, &mut proc, |cp| {
        cp.completed_runs("short.csv").len() == 2
    });
    proc.signal(signal);
    let status = proc.wait(TIMEOUT);
    let outcome = Outcome {
        status,
        output: proc.output(),
    };
    assert!(
        outcome.failed_gracefully(),
        "first {signal}: must exit with a non-zero code, got {:?}:\n{}",
        outcome.status,
        outcome.output
    );
    assert_mentions(&outcome, &["--resume"]);

    let cp = checkpoint::load(&scratch.checkpoint_path()).expect("checkpoint after the signal");
    assert_eq!(cp.fingerprint, fingerprint(&configs));
    let jobs = run_major_plan(&configs).len();
    let done = total_rows(&cp);
    assert!((9..jobs).contains(&done), "{done} of {jobs} jobs done");
    assert_recorded_rows_are_genuine(&cp, slow_rows());
    // CSVs exist exactly for the complete configs, with the genuine content
    let expected = expected_csvs(slow_rows(), &[]);
    let complete: BTreeSet<String> = cp
        .configs
        .iter()
        .filter(|(_, p)| p.rows.len() as u32 == p.runs)
        .map(|(csv, _)| csv.clone())
        .collect();
    assert!(complete.contains("short.csv"));
    assert_eq!(scratch.csv_files(), complete);
    for csv in &complete {
        assert_eq!(
            &fs::read_to_string(scratch.results().join(csv)).unwrap(),
            &expected[csv],
            "{csv}"
        );
    }

    let outcome = run(
        &scratch,
        &["--resume"],
        &[("MAX_CONCURRENT_SIMULATIONS", "3")],
        None,
    );
    assert_success(&outcome);
    assert_results_are(&scratch, &expected);
}

/// Time the long run of the graceful-stop test takes in any build: longer than
/// everything the test does before it lets the run finish.
const LONG_RUN: Duration = Duration::from_secs(6);

const CTRL_C_AGAIN: &str = "Ctrl+C again";
const DEADLINE_REACHED: &str = "--stop-at deadline is reached";

/// Waits (up to [`TIMEOUT`]) until the combined output contains `needle`. The
/// tests wait for what the binary prints rather than for a fixed time, so a
/// slow or oversubscribed machine only makes them slower.
fn wait_for_output(proc: &mut Proc, needle: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !proc.output().contains(needle) {
        if let Some(status) = proc.try_status() {
            panic!(
                "the process ended ({status:?}) without printing {needle:?}:\n{}",
                proc.output()
            );
        }
        assert!(
            Instant::now() < deadline,
            "no {needle:?} within {TIMEOUT:?}:\n{}",
            proc.output()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// A sweep on two workers with a run of `long_s` simulated seconds in flight
/// on one of them. Returns once that is certain.
///
/// Jobs are handed out in run-major order, so the worker that takes the first
/// job (`long.csv` run 0) has started it before the other worker takes
/// `marker.csv` run 0, and the checkpoint written when that run completes
/// proves it. The other worker then carries on with the rest of `marker.csv`,
/// `marker_runs` tiny runs in all (a million, so they cannot run out, if the
/// worker is to keep going until a `--stop-at` deadline), and exits when there
/// are none left.
fn sweep_with_a_long_run_in_flight(
    scratch_name: &str,
    long_s: f64,
    marker_runs: u32,
    extra_args: &[&str],
    envs: &[(&str, &str)],
) -> (Scratch, Proc) {
    let text = to_text(&[
        config_json("long.csv", 1, long_s, true, 42, with_spacer),
        config_json("marker.csv", marker_runs, 0.0004, false, 41, no_extras),
    ]);
    let scratch = Scratch::new(scratch_name, &text);
    let mut proc = Proc::spawn(
        {
            let mut c = scratch.command(extra_args);
            c.env("MAX_CONCURRENT_SIMULATIONS", "2");
            for (key, value) in envs {
                c.env(key, value);
            }
            c
        },
        None,
    );
    wait_for_checkpoint(&scratch, &mut proc, |_| true);
    (scratch, proc)
}

/// [`sweep_with_a_long_run_in_flight`] whose long run never ends.
fn start_long_sweep(scratch_name: &str) -> (Scratch, Proc) {
    sweep_with_a_long_run_in_flight(scratch_name, NEVER_ENDS_S, 1, &[], &[])
}

/// Sends `signal` and returns once the process has reacted to it, i.e. has
/// printed that it waits for the runs in flight.
fn signal_and_wait_for_the_reaction(proc: &mut Proc, signal: &str) {
    proc.signal(signal);
    wait_for_output(proc, CTRL_C_AGAIN);
}

/// Sends the second SIGINT and checks that the process exits at once with
/// 128 + SIGINT, long before any run could have finished.
fn assert_second_sigint_exits_immediately(proc: &mut Proc) {
    proc.signal("INT");
    let waited = Instant::now();
    let status = proc.wait(Duration::from_secs(20));
    assert!(
        waited.elapsed() < Duration::from_secs(5),
        "the second SIGINT must exit immediately, took {:?}",
        waited.elapsed()
    );
    assert_eq!(status.code(), Some(130), "{status:?}");
}

#[test]
fn second_ctrl_c_exits_immediately_while_a_run_is_still_in_flight() {
    // The first SIGINT lets the in-flight run finish (the process stays
    // alive), the second one exits with 128 + SIGINT.
    let (_scratch, mut proc) = start_long_sweep("double-int");
    signal_and_wait_for_the_reaction(&mut proc, "INT");
    assert!(
        proc.try_status().is_none(),
        "the first SIGINT must wait for the in-flight run:\n{}",
        proc.output()
    );
    assert_second_sigint_exits_immediately(&mut proc);
}

#[test_case("INT"; "SIGINT")]
#[test_case("TERM"; "SIGTERM")]
fn the_first_signal_says_that_it_waits_and_that_ctrl_c_again_aborts(signal: &str) {
    let (_scratch, mut proc) = start_long_sweep(&format!("first-signal-message-{signal}"));
    assert!(
        !proc.stderr().contains(CTRL_C_AGAIN),
        "the abort hint belongs to the first signal, not to the start-up:\n{}",
        proc.stderr()
    );
    signal_and_wait_for_the_reaction(&mut proc, signal);
    // printed while the run is still in flight (not only at exit)
    assert!(
        proc.try_status().is_none(),
        "the message must come while waiting, not at exit:\n{}",
        proc.output()
    );
    assert!(
        proc.stderr().contains(CTRL_C_AGAIN),
        "the abort hint belongs on stderr:\n{}",
        proc.output()
    );
    assert!(
        proc.stderr().to_lowercase().contains("waiting"),
        "the message must say that it is waiting for the in-flight runs:\n{}",
        proc.stderr()
    );
    assert!(
        !mentions_the_stop_at_deadline(&proc.output()),
        "no --stop-at was given:\n{}",
        proc.output()
    );
}

/// A sweep whose `--stop-at` deadline (3 s after the launch) has been noticed
/// by the idle worker while a long run of `long_s` simulated seconds is still
/// in flight on the other one. Returns once the process has said so.
fn sweep_after_its_stop_at_deadline(scratch_name: &str, long_s: f64) -> (Scratch, Proc) {
    let (scratch, mut proc) = sweep_with_a_long_run_in_flight(
        scratch_name,
        long_s,
        1_000_000,
        &["--stop-at", "11:00"],
        &[("DPDS_SIM_FAKE_NOW", "10:59:57")],
    );
    wait_for_output(&mut proc, DEADLINE_REACHED);
    (scratch, proc)
}

#[test_case("INT"; "SIGINT")]
#[test_case("TERM"; "SIGTERM")]
fn the_first_signal_after_the_stop_at_deadline_is_still_graceful(signal: &str) {
    let (scratch, mut proc) =
        sweep_after_its_stop_at_deadline(&format!("deadline-{signal}"), duration_for(LONG_RUN));
    assert!(
        proc.try_status().is_none(),
        "the long run must still be in flight:\n{}",
        proc.output()
    );
    signal_and_wait_for_the_reaction(&mut proc, signal);
    assert!(
        proc.try_status().is_none(),
        "a first {signal} after the deadline must still wait for the in-flight run \
         (the deadline's stop flag must not turn it into a second signal):\n{}",
        proc.output()
    );
    let status = proc.wait(TIMEOUT);
    let outcome = Outcome {
        status,
        output: proc.output(),
    };
    assert!(
        outcome.failed_gracefully() && outcome.code() != Some(130),
        "expected the graceful non-zero exit, got {:?}:\n{}",
        outcome.status,
        outcome.output
    );
    assert_mentions(&outcome, &["--resume"]);
    assert!(
        mentions_the_stop_at_deadline(&outcome.output),
        "{}",
        outcome.output
    );

    // the in-flight long run finished and was checkpointed and written
    let cp = checkpoint::load(&scratch.checkpoint_path()).expect("checkpoint after the stop");
    assert_eq!(
        cp.completed_runs("long.csv"),
        [0].into_iter().collect::<BTreeSet<u32>>()
    );
    assert!(!cp.completed_runs("marker.csv").is_empty());
    assert!(
        scratch.csv_files().contains("long.csv"),
        "long.csv is complete"
    );
    assert!(!scratch.csv_files().contains("marker.csv"));
}

#[test]
fn a_second_signal_after_the_stop_at_deadline_exits_immediately() {
    let (scratch, mut proc) = sweep_after_its_stop_at_deadline("deadline-twice", NEVER_ENDS_S);
    signal_and_wait_for_the_reaction(&mut proc, "INT");
    assert!(proc.try_status().is_none(), "{}", proc.output());
    assert_second_sigint_exits_immediately(&mut proc);
    let cp = checkpoint::load(&scratch.checkpoint_path()).expect("checkpoint stays valid");
    assert!(
        cp.completed_runs("long.csv").is_empty(),
        "the run was cut off"
    );
}

// --------------------------------------------------------------------- panics

#[test]
fn a_panic_in_a_worker_names_the_job_keeps_the_checkpoint_and_fails() {
    // One worker, run-major: a.csv run 0 completes, then b.csv run 0 panics
    // (capacity usize::MAX cannot be reserved).
    let text = to_text(&[
        config_json("a.csv", 3, 0.0004, false, 31, no_extras),
        config_json("b.csv", 3, 0.0004, false, 32, |v| {
            v["simulation_config"]["buffer_capacity"] = json!(usize::MAX);
        }),
    ]);
    let configs = parse(&text);
    let scratch = Scratch::new("panic", &text);
    let outcome = run(&scratch, &[], &[("MAX_CONCURRENT_SIMULATIONS", "1")], None);
    assert!(!outcome.status.success(), "{}", outcome.output);
    assert_mentions(&outcome, &["b.csv"]);
    assert!(
        outcome.output.to_lowercase().contains("run"),
        "the message must name the run:\n{}",
        outcome.output
    );
    let cp = checkpoint::load(&scratch.checkpoint_path())
        .expect("completed runs stay checkpointed after a panic");
    assert_eq!(cp.fingerprint, fingerprint(&configs));
    assert_eq!(
        cp.completed_runs("a.csv"),
        [0].into_iter().collect::<BTreeSet<u32>>()
    );
    assert!(cp.completed_runs("b.csv").is_empty());
    let genuine_a = lib::simulation::run_simulation(&configs[0], 0)
        .csv_record(0)
        .to_vec();
    assert_eq!(cp.configs["a.csv"].rows[&0], genuine_a);
    assert!(scratch.csv_files().is_empty());
}

// -------------------------------------------------------------- write failures

/// A failure to write a result file is an expected error: non-zero exit
/// code (not death by a signal such as SIGABRT from a panic in a release
/// build), a message naming the file, and no panic report.
fn assert_clean_write_failure(outcome: &Outcome, file: &str) {
    assert!(
        outcome.failed_gracefully(),
        "must exit with a non-zero code, got {:?}:\n{}",
        outcome.status,
        outcome.output
    );
    assert_mentions(outcome, &[file]);
    assert!(
        !outcome.output.contains("panicked"),
        "a write failure is an error message, not a panic:\n{}",
        outcome.output
    );
}

#[test]
fn a_checkpoint_that_cannot_be_saved_ends_the_sweep_with_an_error_naming_the_file() {
    // Many tiny runs on two workers. Once the sweep is running, a directory
    // takes the place of `.checkpoint.json`: renaming the temp file over it
    // fails for every user (a read-only directory would not stop root).
    let text = to_text(&[config_json(
        "many.csv", 100_000, 0.0004, false, 51, no_extras,
    )]);
    let scratch = Scratch::new("save-fails", &text);
    let mut proc = Proc::spawn(
        {
            let mut c = scratch.command(&[]);
            c.env("MAX_CONCURRENT_SIMULATIONS", "2");
            c
        },
        None,
    );
    wait_for_checkpoint(&scratch, &mut proc, |_| true);
    let path = scratch.checkpoint_path();
    let deadline = Instant::now() + TIMEOUT;
    // the sweep may rename a new file in between: try again
    while {
        let _ = fs::remove_file(&path);
        fs::create_dir(&path).is_err()
    } {
        assert!(Instant::now() < deadline, "cannot sabotage the checkpoint");
    }
    let status = proc.wait(Duration::from_secs(30));
    assert_clean_write_failure(
        &Outcome {
            status,
            output: proc.output(),
        },
        ".checkpoint.json",
    );
}

#[test]
fn a_csv_that_cannot_be_written_ends_the_sweep_with_an_error_naming_the_file() {
    // `results/done.csv` is a directory: the atomic write cannot replace it.
    // The runs are checkpointed before the CSV is written, so nothing is lost.
    let text = to_text(&[
        config_json("done.csv", 2, 0.0004, false, 61, no_extras),
        config_json("other.csv", 2, 0.0004, false, 62, no_extras),
    ]);
    let scratch = Scratch::new("csv-fails", &text);
    fs::create_dir_all(scratch.results().join("done.csv")).unwrap();
    let outcome = run(&scratch, &[], &[("MAX_CONCURRENT_SIMULATIONS", "1")], None);
    assert_clean_write_failure(&outcome, "done.csv");
    let cp = checkpoint::load(&scratch.checkpoint_path())
        .expect("the checkpoint stays valid after a failed CSV write");
    assert_eq!(
        cp.completed_runs("done.csv"),
        [0, 1].into_iter().collect::<BTreeSet<u32>>()
    );
}
