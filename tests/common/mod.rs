//! Helpers shared by the integration tests that drive the `dpds_simulation`
//! binary. Every function here is used by both `sweep_driver.rs` and
//! `model.rs` (unused helpers would trip `-D warnings`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Runs the `dpds_simulation` binary in a fresh scratch directory containing
/// `simulation_configs.json` (= `configs_json`) and returns the `results/`
/// directory it produced. `workers` sets `MAX_CONCURRENT_SIMULATIONS`
/// (`None` removes the variable). Panics if the binary fails.
pub fn run_sweep(name: &str, configs_json: &str, workers: Option<&str>) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("dpds-simulation-tests")
        .join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    fs::write(dir.join("simulation_configs.json"), configs_json).expect("write configs");

    let mut command = Command::new(env!("CARGO_BIN_EXE_dpds_simulation"));
    command.current_dir(&dir);
    match workers {
        Some(w) => command.env("MAX_CONCURRENT_SIMULATIONS", w),
        None => command.env_remove("MAX_CONCURRENT_SIMULATIONS"),
    };
    let output = command.output().expect("spawn dpds_simulation");
    assert!(
        output.status.success(),
        "dpds_simulation failed ({:?}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    dir.join("results")
}

/// Reads a CSV file into its header and its rows.
pub fn read_csv(path: &Path) -> (Vec<String>, Vec<Vec<String>>) {
    let mut reader = csv::Reader::from_path(path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let header = reader
        .headers()
        .expect("csv header")
        .iter()
        .map(str::to_owned)
        .collect();
    let rows = reader
        .records()
        .map(|r| r.expect("csv row").iter().map(str::to_owned).collect())
        .collect();
    (header, rows)
}
