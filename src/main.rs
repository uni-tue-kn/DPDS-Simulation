//! Sweep driver: runs every `(config, run)` job of `simulation_configs.json`
//! on a rayon pool and checkpoints after every completed run, so an
//! interrupted sweep can be resumed (see the README).

use lib::{
    checkpoint::{self, Checkpoint, ConfigProgress},
    config::Config,
    results::csv_bytes,
    simulation::{arrivals_per_run, run_simulation},
};

use chrono::Timelike;
use indicatif::{ProgressBar, ProgressStyle};
use signal_hook::consts::{SIGINT, SIGTERM};
use std::cell::RefCell;
use std::env;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RESULT_DIR: &str = "results";
const CONFIG_FILE: &str = "simulation_configs.json";
/// Exit code after a second Ctrl+C (128 + SIGINT).
const SECOND_SIGNAL_EXIT_CODE: i32 = 130;
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

const USAGE: &str = "usage: dpds_simulation [--resume | --fresh] [--dry-run] \
[--stop-at HH:MM] [--start-at HH:MM]";

// ----------------------------------------------------------------- arguments

#[derive(Debug, Default)]
struct Args {
    resume: bool,
    fresh: bool,
    dry_run: bool,
    /// Time of day (local), as the offset from midnight.
    stop_at: Option<Duration>,
    start_at: Option<Duration>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--resume" => parsed.resume = true,
            "--fresh" => parsed.fresh = true,
            "--dry-run" => parsed.dry_run = true,
            "--stop-at" | "--start-at" => {
                let value = args.next().ok_or(format!("{arg} needs a value HH:MM"))?;
                let time = parse_time_of_day(&value).ok_or(format!(
                    "{arg}: {value:?} is not a time HH:MM (00:00-23:59)"
                ))?;
                if arg == "--stop-at" {
                    parsed.stop_at = Some(time);
                } else {
                    parsed.start_at = Some(time);
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                exit(0);
            }
            _ => return Err(format!("unknown argument {arg:?}\n{USAGE}")),
        }
    }
    if parsed.resume && parsed.fresh {
        return Err("--resume and --fresh are mutually exclusive".to_string());
    }
    Ok(parsed)
}

/// `H:MM`/`HH:MM` (one or two digits each, hour 0-23, minute 0-59).
fn parse_time_of_day(text: &str) -> Option<Duration> {
    let (hours, minutes) = text.split_once(':')?;
    let number = |s: &str| {
        (matches!(s.len(), 1 | 2) && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u64>().ok())
            .flatten()
    };
    let (hours, minutes) = (number(hours)?, number(minutes)?);
    (hours < 24 && minutes < 60).then(|| Duration::from_secs(hours * 3600 + minutes * 60))
}

/// The local time of day now. `DPDS_SIM_FAKE_NOW=HH:MM:SS` replaces it (a
/// test seam for the time flags).
fn time_of_day_now() -> Duration {
    if let Ok(fake) = env::var("DPDS_SIM_FAKE_NOW") {
        let parts: Vec<u64> = fake.split(':').filter_map(|p| p.parse().ok()).collect();
        if let [h, m, s] = parts[..] {
            return Duration::from_secs(h * 3600 + m * 60 + s);
        }
        fail(&format!("DPDS_SIM_FAKE_NOW must be HH:MM:SS, got {fake:?}"));
    }
    let now = chrono::Local::now().time();
    Duration::new(u64::from(now.num_seconds_from_midnight()), now.nanosecond())
}

/// How long until the next occurrence of `target` (a time that already
/// passed, or is now, means tomorrow).
fn time_until(target: Duration, now: Duration) -> Duration {
    if target > now {
        target - now
    } else {
        target + DAY - now
    }
}

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    exit(1)
}

// ------------------------------------------------------------------ plan/jobs

/// Run-major job list over the runs not yet in the checkpoint: run 0 of
/// every config, then run 1 of every config that has one, and so on.
fn pending_jobs(configs: &[Config], checkpoint: &Checkpoint) -> Vec<(usize, u32)> {
    let max_runs = configs
        .iter()
        .map(|c| c.simulation_config.runs)
        .max()
        .unwrap_or(0);
    (0..max_runs)
        .flat_map(|run| {
            configs
                .iter()
                .enumerate()
                .filter(move |(_, c)| c.simulation_config.runs > run)
                .map(move |(i, _)| (i, run))
        })
        .filter(|&(i, run)| {
            let csv = &configs[i].simulation_config.csv_output_path;
            !checkpoint.configs[csv].rows.contains_key(&run)
        })
        .collect()
}

fn print_dry_run(configs: &[Config], checkpoint: Option<&Checkpoint>) {
    let mut jobs = 0;
    let mut packets = 0;
    println!("Sweep plan:");
    for config in configs {
        let sim = &config.simulation_config;
        let per_run = arrivals_per_run(sim);
        println!(
            "  {}: {} runs x {per_run} packets",
            sim.csv_output_path, sim.runs
        );
        jobs += u64::from(sim.runs);
        packets += u64::from(sim.runs) * per_run;
    }
    println!(
        "{} configs, {jobs} jobs, about {packets} simulated packets",
        configs.len()
    );
    match checkpoint {
        Some(cp) => {
            let (done, total) = cp.progress();
            println!("Checkpoint: {done}/{total} runs completed");
        }
        None => println!("Checkpoint: none"),
    }
}

// --------------------------------------------------------- checkpoint at start

fn checkpoint_path() -> PathBuf {
    Path::new(RESULT_DIR).join(checkpoint::FILE_NAME)
}

const UNUSABLE_HINT: &str =
    "Fix the problem, or start over with --fresh (this deletes the checkpoint).";

/// Loads the checkpoint and checks that it belongs to `configs`: same
/// fingerprint, and (should the file have been edited) the same config keys
/// with the same run counts. The error names the file and `--fresh`.
fn load_matching(path: &Path, configs: &[Config]) -> Result<Checkpoint, String> {
    let unusable = |what: String| format!("{}: {what}\n{UNUSABLE_HINT}", path.display());
    let loaded = checkpoint::load(path).map_err(|e| format!("{e}\n{UNUSABLE_HINT}"))?;
    if loaded.fingerprint != checkpoint::fingerprint(configs) {
        return Err(unusable(format!(
            "the checkpoint was made for other configs than {CONFIG_FILE}"
        )));
    }
    for config in configs {
        let sim = &config.simulation_config;
        match loaded.configs.get(&sim.csv_output_path) {
            Some(progress) if progress.runs == sim.runs => {}
            Some(progress) => {
                return Err(unusable(format!(
                    "{} has {} runs in the checkpoint but {} in {CONFIG_FILE}",
                    sim.csv_output_path, progress.runs, sim.runs
                )));
            }
            None => {
                return Err(unusable(format!(
                    "{} is missing in the checkpoint",
                    sim.csv_output_path
                )));
            }
        }
    }
    if loaded.configs.len() != configs.len() {
        return Err(unusable(
            "the checkpoint has configs that are not in the current configs".to_string(),
        ));
    }
    Ok(loaded)
}

fn describe(cp: &Checkpoint) -> String {
    let (done, total) = cp.progress();
    let age_s = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .saturating_sub(cp.updated_unix_s);
    format!(
        "{done}/{total} runs completed in {} configs, last updated {}h {:02}m ago",
        cp.configs.len(),
        age_s / 3600,
        age_s % 3600 / 60
    )
}

/// `y`/`n` on stdin, asked again after anything else. EOF is an error.
fn ask(question: &str) -> Result<bool, String> {
    let mut stdin = io::stdin().lock();
    loop {
        print!("{question} [y/n] ");
        io::stdout().flush().ok();
        let mut line = String::new();
        let n = stdin
            .read_line(&mut line)
            .map_err(|e| format!("cannot read the answer: {e}"))?;
        if n == 0 {
            return Err(format!("no answer on stdin to \"{question}\""));
        }
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Please answer y or n."),
        }
    }
}

/// Decides how to start: an existing checkpoint is resumed, or deleted
/// (`--fresh` or the prompt), or the start is refused with an error. A
/// checkpoint that cannot be used is never replaced silently.
fn start_checkpoint(args: &Args, configs: &[Config]) -> Result<Checkpoint, String> {
    let path = checkpoint_path();
    if !path.exists() {
        return Ok(Checkpoint::new(configs));
    }
    if args.fresh {
        remove_checkpoint(&path)?;
        return Ok(Checkpoint::new(configs));
    }
    let loaded = load_matching(&path, configs)?;
    println!("Found checkpoint {}: {}", path.display(), describe(&loaded));
    if args.resume {
        return Ok(loaded);
    }
    let interactive =
        io::stdin().is_terminal() || env::var("DPDS_SIM_FORCE_PROMPT").is_ok_and(|v| v == "1");
    if !interactive {
        return Err("a checkpoint exists and stdin is not a terminal: \
pass --resume to continue it or --fresh to delete it and start over"
            .to_string());
    }
    if ask("Resume it?")? {
        return Ok(loaded);
    }
    if ask("Really delete the checkpoint and start fresh?")? {
        remove_checkpoint(&path)?;
        return Ok(Checkpoint::new(configs));
    }
    Err("checkpoint left untouched".to_string())
}

fn remove_checkpoint(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot delete {}: {e}", path.display()))
        }
        _ => Ok(()),
    }
}

// ------------------------------------------------------------------------ sweep

thread_local! {
    /// The job this thread is simulating, for the panic message.
    static CURRENT_JOB: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// A panic inside a simulation names its job and ends the process (also with
/// `panic = "abort"`, where unwinding cannot be relied on). Runs completed
/// before are already checkpointed.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        match CURRENT_JOB.with(|job| job.borrow_mut().take()) {
            Some(job) => {
                eprintln!("\nerror: {job} panicked: {info}");
                eprintln!(
                    "Completed runs are checkpointed; fix the problem and rerun with --resume."
                );
                exit(1)
            }
            None => default_hook(info),
        }
    }));
}

struct Sweep<'a> {
    configs: &'a [Config],
    jobs: Vec<(usize, u32)>,
    next_job: AtomicUsize,
    /// Set by the first SIGINT/SIGTERM.
    signalled: Arc<AtomicBool>,
    /// Set once the `--stop-at` deadline has passed (independent of signals).
    deadline_hit: AtomicBool,
    stop_deadline: Option<Instant>,
    checkpoint: Mutex<Checkpoint>,
    progress: ProgressBar,
}

impl Sweep<'_> {
    /// One worker: takes jobs in order until none are left or a stop is
    /// requested. A job that was started is always finished and checkpointed.
    fn work(&self) {
        loop {
            if self.stop_requested() {
                return;
            }
            let Some(&(config_index, run)) =
                self.jobs.get(self.next_job.fetch_add(1, Ordering::Relaxed))
            else {
                return;
            };
            let config = &self.configs[config_index];
            let csv = &config.simulation_config.csv_output_path;
            let job = format!("{csv} run {run}");
            self.progress.set_message(job.clone());

            CURRENT_JOB.with(|current| *current.borrow_mut() = Some(job));
            let row = run_simulation(config, run).csv_record(run).to_vec();
            CURRENT_JOB.with(|current| *current.borrow_mut() = None);

            self.complete(csv, run, row);
            self.progress.inc(1);
        }
    }

    /// A signal arrived or the `--stop-at` deadline passed while jobs were
    /// still to be handed out.
    fn stop_requested(&self) -> bool {
        if self.signalled.load(Ordering::Relaxed) || self.deadline_hit.load(Ordering::Relaxed) {
            return true;
        }
        let jobs_left = self.next_job.load(Ordering::Relaxed) < self.jobs.len();
        if jobs_left
            && self.stop_deadline.is_some_and(|d| Instant::now() >= d)
            && !self.deadline_hit.swap(true, Ordering::Relaxed)
        {
            eprintln!(
                "\nThe --stop-at deadline is reached: no new runs are started, waiting for \
the runs in flight to finish and be checkpointed."
            );
            return true;
        }
        false
    }

    /// Records the run, saves the checkpoint and, if the config is now
    /// complete, writes its CSV.
    fn complete(&self, csv: &str, run: u32, row: Vec<String>) {
        let mut checkpoint = self
            .checkpoint
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        checkpoint.record(csv, run, row);
        let path = checkpoint_path();
        checkpoint::save(&path, &checkpoint)
            .unwrap_or_else(|e| fail(&format!("cannot save {}: {e}", path.display())));
        if checkpoint.configs[csv].is_complete() {
            write_csv(csv, &checkpoint.configs[csv]);
        }
    }
}

/// Writes the CSV of a config from its checkpointed rows (sorted by run). A
/// failure ends the process with an error message (runs are checkpointed).
fn write_csv(csv: &str, progress: &ConfigProgress) {
    let path = Path::new(RESULT_DIR).join(csv);
    let bytes = csv_bytes(progress.rows.values())
        .unwrap_or_else(|e| fail(&format!("cannot build {}: {e}", path.display())));
    checkpoint::write_atomic(&path, &bytes)
        .unwrap_or_else(|e| fail(&format!("cannot write {}: {e}", path.display())));
}

/// `MAX_CONCURRENT_SIMULATIONS` if it is a positive integer, otherwise the
/// available parallelism.
fn worker_count() -> usize {
    env::var("MAX_CONCURRENT_SIMULATIONS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, |n| n.get()))
}

/// Waits until `delay` has passed; returns false if a signal arrived.
fn wait(delay: Duration, signalled: &AtomicBool) -> bool {
    let end = Instant::now() + delay;
    while Instant::now() < end {
        if signalled.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(100).min(end - Instant::now()));
    }
    true
}

fn main() {
    let args = parse_args(env::args().skip(1)).unwrap_or_else(|e| fail(&e));

    let config_text = fs::read_to_string(CONFIG_FILE)
        .unwrap_or_else(|e| fail(&format!("cannot read {CONFIG_FILE}: {e}")));
    let configs: Vec<Config> = serde_json::from_str(&config_text)
        .unwrap_or_else(|e| fail(&format!("cannot parse {CONFIG_FILE}: {e}")));
    let mut csv_paths: Vec<&str> = configs
        .iter()
        .map(|c| c.simulation_config.csv_output_path.as_str())
        .collect();
    csv_paths.sort_unstable();
    if let Some(pair) = csv_paths.windows(2).find(|pair| pair[0] == pair[1]) {
        fail(&format!(
            "{CONFIG_FILE}: csv_output_path {:?} is used twice",
            pair[0]
        ));
    }

    if args.dry_run {
        let path = checkpoint_path();
        let checkpoint = path
            .exists()
            .then(|| load_matching(&path, &configs).unwrap_or_else(|e| fail(&e)));
        print_dry_run(&configs, checkpoint.as_ref());
        return;
    }

    let checkpoint = start_checkpoint(&args, &configs).unwrap_or_else(|e| fail(&e));

    // Set by the first SIGINT/SIGTERM, and only by a signal.
    let signalled = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        // Registered in this order: a signal that arrives while `signalled`
        // is already set exits at once (second Ctrl+C), the first one sets it.
        signal_hook::flag::register_conditional_shutdown(
            signal,
            SECOND_SIGNAL_EXIT_CODE,
            Arc::clone(&signalled),
        )
        .and_then(|_| signal_hook::flag::register(signal, Arc::clone(&signalled)))
        .unwrap_or_else(|e| fail(&format!("cannot install the signal handler: {e}")));
    }

    let mut now = time_of_day_now();
    if let Some(start_at) = args.start_at {
        let delay = time_until(start_at, now);
        println!("Waiting {}s until --start-at.", delay.as_secs());
        if !wait(delay, &signalled) {
            fail("interrupted while waiting for --start-at; nothing was run");
        }
        // The sweep starts at --start-at: --stop-at counts from there.
        now = start_at;
    }
    let stop_deadline = args.stop_at.map(|t| Instant::now() + time_until(t, now));

    fs::create_dir_all(RESULT_DIR)
        .unwrap_or_else(|e| fail(&format!("cannot create {RESULT_DIR}: {e}")));
    install_panic_hook();

    // Configs that are already complete (a resumed checkpoint, or no runs).
    for (csv, progress) in checkpoint.configs.iter().filter(|(_, p)| p.is_complete()) {
        write_csv(csv, progress);
    }

    let jobs = pending_jobs(&configs, &checkpoint);
    let (done, total) = checkpoint.progress();
    let progress = ProgressBar::new(total as u64);
    progress.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} (ETA {eta}) {msg}",
            )
            .expect("valid progress template")
            .progress_chars("#>-"),
    );
    progress.set_position(done as u64);
    // The ETA must not count the resumed runs as done in no time.
    progress.reset_eta();

    let sweep = Sweep {
        configs: &configs,
        jobs,
        next_job: AtomicUsize::new(0),
        signalled,
        deadline_hit: AtomicBool::new(false),
        stop_deadline,
        checkpoint: Mutex::new(checkpoint),
        progress,
    };
    if !sweep.jobs.is_empty() {
        // The signal handler only sets a flag: this thread prints.
        let signalled = Arc::clone(&sweep.signalled);
        thread::spawn(move || {
            while !signalled.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(50));
            }
            eprintln!(
                "\nInterrupted: waiting for the runs in flight to finish and be checkpointed. \
Press Ctrl+C again to exit immediately."
            );
        });
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(worker_count().min(sweep.jobs.len()))
            .build()
            .unwrap_or_else(|e| fail(&format!("cannot start the worker pool: {e}")));
        pool.broadcast(|_| sweep.work());
    }

    let checkpoint = sweep
        .checkpoint
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (done, total) = checkpoint.progress();
    if done < total {
        sweep.progress.abandon();
        eprintln!(
            "Stopped with {done}/{total} runs completed. Continue with: dpds_simulation --resume"
        );
        exit(1);
    }
    remove_checkpoint(&checkpoint_path()).unwrap_or_else(|e| fail(&e));
    sweep
        .progress
        .finish_with_message("All simulations completed");
}
