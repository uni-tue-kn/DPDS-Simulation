//! End-to-end tests of the `dpds_simulation` binary (sweep driver) and of the
//! release profile. They only use the CSV files the binary writes.
//!
//! Spec requirements covered: one CSV per config, header + one row per run,
//! rows sorted by run index, results independent of the worker count
//! (`MAX_CONCURRENT_SIMULATIONS`, fixed seed => byte-identical files), the
//! renamed/new drop columns, the release profile in `Cargo.toml`, and static
//! checks of the repository (R scripts, dependencies, README). The
//! checkpoint/resume behaviour is tested in `checkpoint_cli.rs`.

mod common;

use common::{read_csv, run_sweep};
use std::fs;

const RUNS: u32 = 7;

/// Three small configs (different features) with a fixed seed. `seed` is
/// ignored by the pre-redesign binary (unknown JSON fields are skipped).
fn configs_json() -> String {
    format!(
        r#"[
  {{"simulation_config": {{"duration_s": 0.0004, "runs": {RUNS}, "rate_gbps": 1.2,
      "average_frame_size_byte": 1480, "activate_reordering": false,
      "csv_output_path": "a_fifo.csv", "seed": 100}},
    "delay_config": {{"mean_ns": 100000, "std_ns": 30000, "rv_distribution": "NormalDistribution",
      "activate_correlation": false, "half_life_period_ns": 0}}}},
  {{"simulation_config": {{"duration_s": 0.0004, "runs": {RUNS}, "rate_gbps": 1.2,
      "average_frame_size_byte": 1480, "activate_reordering": true,
      "csv_output_path": "b_reorder.csv", "seed": 200}},
    "delay_config": {{"mean_ns": 100000, "std_ns": 30000, "rv_distribution": "NormalDistribution",
      "activate_correlation": false, "half_life_period_ns": 0}}}},
  {{"simulation_config": {{"duration_s": 0.0004, "runs": {RUNS}, "rate_gbps": 1.2,
      "average_frame_size_byte": 1480, "activate_reordering": false,
      "csv_output_path": "c_spacer.csv", "seed": 300}},
    "delay_config": {{"mean_ns": 100000, "std_ns": 30000, "rv_distribution": "UniformDistribution",
      "activate_correlation": false, "half_life_period_ns": 0}},
    "limiter_config": {{"mode": "Spacer", "rate_bps": 800000000, "limit_bit": 1000000}}}}
]"#
    )
}

const FILES: [&str; 3] = ["a_fifo.csv", "b_reorder.csv", "c_spacer.csv"];

/// Columns that exist before and after the redesign, in this order at the
/// start of the header.
const COMMON_PREFIX: [&str; 16] = [
    "run",
    "tracked_packets",
    "inter_arrival_time_mean_ns",
    "inter_arrival_time_std_ns",
    "delay_mean_ns",
    "delay_std_ns",
    "kendall_tau_distance",
    "kendall_tau_distance_div_n",
    "out_of_order_packets",
    "out_of_order_packets_div_n",
    "tracked_theoretical_passage_times",
    "theoretical_passage_time_mean_ns",
    "theoretical_passage_time_std_ns",
    "tracked_measured_passage_times",
    "measured_passage_time_mean_ns",
    "measured_passage_time_std_ns",
];

#[test]
fn every_config_gets_its_own_complete_csv() {
    let results = run_sweep("driver-complete", &configs_json(), Some("3"));
    for file in FILES {
        let (header, rows) = read_csv(&results.join(file));
        assert_eq!(rows.len(), RUNS as usize, "{file}");
        for row in &rows {
            assert_eq!(row.len(), header.len(), "{file}: row shape");
        }
    }
}

#[test]
fn csv_header_keeps_the_unchanged_columns() {
    let results = run_sweep("driver-header", &configs_json(), Some("2"));
    for file in FILES {
        let (header, _) = read_csv(&results.join(file));
        assert_eq!(header[..COMMON_PREFIX.len()], COMMON_PREFIX, "{file}");
        for name in [
            "burst_spacing_offset_mean_ns",
            "burst_spacing_offset_std_ns",
            "burst_spacing_offset_max_ns",
            "dropped_packets_burst_virtual_queue",
        ] {
            assert!(header.iter().any(|h| h == name), "{file}: missing {name}");
        }
    }
}

#[test]
fn csv_header_has_the_new_drop_columns_and_no_bandwidth_column() {
    let results = run_sweep("driver-dropcols", &configs_json(), Some("2"));
    let (header, _) = read_csv(&results.join("a_fifo.csv"));
    for name in [
        "dropped_packets_loss",
        "dropped_packets_limiter",
        "dropped_packets_buffer_full",
    ] {
        assert!(header.iter().any(|h| h == name), "missing {name}");
    }
    assert!(!header.iter().any(|h| h == "dropped_packets_bandwidth"));
}

#[test]
fn rows_are_sorted_by_run_index_for_any_worker_count() {
    for workers in ["1", "3", "8"] {
        let results = run_sweep(
            &format!("driver-sorted-{workers}"),
            &configs_json(),
            Some(workers),
        );
        for file in FILES {
            let (_, rows) = read_csv(&results.join(file));
            let runs: Vec<u32> = rows.iter().map(|r| r[0].parse().unwrap()).collect();
            assert_eq!(
                runs,
                (0..RUNS).collect::<Vec<_>>(),
                "{file} with {workers} workers"
            );
        }
    }
}

#[test]
fn seeded_results_do_not_depend_on_the_worker_count() {
    // 1 worker vs 3 workers vs default vs more workers than jobs must produce
    // byte-identical files.
    let reference = run_sweep("driver-w1", &configs_json(), Some("1"));
    for (name, workers) in [
        ("driver-w3", Some("3")),
        ("driver-w64", Some("64")),
        ("driver-wdefault", None),
    ] {
        let other = run_sweep(name, &configs_json(), workers);
        for file in FILES {
            let a = fs::read_to_string(reference.join(file)).unwrap();
            let b = fs::read_to_string(other.join(file)).unwrap();
            assert_eq!(a, b, "{file}: {name} differs from 1 worker");
        }
    }
}

#[test]
fn invalid_max_concurrent_simulations_falls_back_to_the_default() {
    // An unparsable value is ignored (as before); the sweep still completes
    // and, being seeded, gives the same files as the default.
    let invalid = run_sweep("driver-invalid-env", &configs_json(), Some("not-a-number"));
    let default = run_sweep("driver-default-env", &configs_json(), None);
    for file in FILES {
        let (_, rows) = read_csv(&invalid.join(file));
        assert_eq!(rows.len(), RUNS as usize, "{file}");
        assert_eq!(
            fs::read_to_string(invalid.join(file)).unwrap(),
            fs::read_to_string(default.join(file)).unwrap(),
            "{file}"
        );
    }
}

#[test]
fn different_seeds_per_run_give_different_rows() {
    // Run r uses seed + r, so the 7 rows of a config are not copies. (Passes
    // vacuously before the redesign because runs are unseeded there.)
    let results = run_sweep("driver-run-seeds", &configs_json(), Some("2"));
    let (_, rows) = read_csv(&results.join("a_fifo.csv"));
    let mut distinct: Vec<&Vec<String>> = Vec::new();
    for row in &rows {
        // compare everything except the run index
        if !distinct.iter().any(|d| d[1..] == row[1..]) {
            distinct.push(row);
        }
    }
    assert!(
        distinct.len() > 1,
        "all runs identical: seed + r not applied"
    );
}

#[test]
fn release_profile_has_lto_single_codegen_unit_and_abort() {
    let manifest = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    let section = manifest
        .split("[profile.release]")
        .nth(1)
        .expect("[profile.release] section missing")
        .split("\n[")
        .next()
        .unwrap()
        .replace(' ', "");
    assert!(section.contains("lto=\"fat\""), "{section}");
    assert!(section.contains("codegen-units=1"), "{section}");
    assert!(section.contains("panic=\"abort\""), "{section}");
}

// ------------------------------------------------------------ static checks

fn read_repo_file(relative: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

fn r_scripts() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("results");
    let mut scripts: Vec<(String, String)> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "r"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read_to_string(&p).unwrap(),
            )
        })
        .collect();
    scripts.sort();
    scripts
}

#[test]
fn no_r_script_reads_the_removed_dropped_packets_bandwidth_column() {
    // H2: the CSV column is `dropped_packets_limiter` now
    let scripts = r_scripts();
    assert!(scripts.len() >= 5, "found only {} R scripts", scripts.len());
    for (name, text) in &scripts {
        assert!(
            !text.contains("dropped_packets_bandwidth"),
            "{name} still references dropped_packets_bandwidth"
        );
    }
}

#[test]
fn the_plot_scripts_that_check_for_drops_use_the_limiter_column() {
    let scripts = r_scripts();
    for wanted in [
        "plot_delays.r",
        "plot_distributions.r",
        "plot_spacing.r",
        "plot_half_life.r",
        "plot_results.r",
    ] {
        let (_, text) = scripts
            .iter()
            .find(|(name, _)| name == wanted)
            .unwrap_or_else(|| panic!("results/{wanted} missing"));
        assert!(
            text.contains("dropped_packets_limiter"),
            "{wanted} must read dropped_packets_limiter"
        );
    }
}

#[test]
fn worker_pool_and_signals_use_rayon_and_signal_hook_or_ctrlc_without_tokio() {
    let manifest = read_repo_file("Cargo.toml");
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("[dependencies] section")
        .split("\n[")
        .next()
        .unwrap();
    let has = |name: &str| {
        dependencies
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{name} =")))
    };
    assert!(has("rayon"), "rayon must be a dependency:\n{dependencies}");
    assert!(
        has("signal-hook") || has("ctrlc"),
        "signal-hook or ctrlc must be a dependency:\n{dependencies}"
    );
    assert!(!has("tokio"), "the work is CPU-bound: no tokio");
}

#[test]
fn readme_documents_the_checkpoint_flags_and_the_buffer_capacity_default() {
    let readme = read_repo_file("README.md");
    for needle in [
        "--resume",
        "--fresh",
        "--dry-run",
        "--stop-at",
        "--start-at",
        ".checkpoint.json",
        "2^21",
    ] {
        assert!(readme.contains(needle), "README must mention {needle}");
    }
    // M2: the default is 2^21 = 2097152 everywhere, never 2^20 or 2^24
    let example = readme
        .lines()
        .find(|l| l.contains("\"buffer_capacity\":"))
        .expect("README config example shows buffer_capacity");
    assert!(example.contains("2^21"), "{example}");
    assert!(example.contains("2097152"), "{example}");
    assert!(
        !example.contains("2^20") && !example.contains("2^24"),
        "{example}"
    );
    let section = readme
        .split("### Buffer capacity")
        .nth(1)
        .expect("README has a Buffer capacity section")
        .split("\n#")
        .next()
        .unwrap();
    assert!(section.contains("2^21"), "{section}");
    assert!(
        !section.contains("2^20") && !section.contains("2^24"),
        "{section}"
    );
    for line in readme.lines() {
        assert!(!line.contains("2^20"), "stale 2^20: {line}");
        // the old default may only be named as the one that was replaced
        assert!(
            !line.contains("2^24") || line.contains("instead of 2^24"),
            "stale 2^24: {line}"
        );
    }
}
