//! Standalone sampling-throughput benchmark.
//!
//! Measures how fast the delay distributions can be sampled in real (wall-clock)
//! time, independent of the discrete-event simulation. For each distribution it
//! draws exactly N samples through `Delayer::delayer_sample_ns` (the same path the
//! emulator uses) with correlation disabled, so the measurement isolates pure
//! random-variate generation.
//!
//! Settings: 10 ms mean delay, 3 ms jitter (std), correlation off. Each
//! distribution draws a fixed 1,000,000,000 samples, and every experiment is
//! repeated 10 times so the throughput numbers carry statistical significance.
//!
//! Usage:
//!   dpds_sampling_benchmark [N_SAMPLES] [RUNS]
//!
//! N_SAMPLES defaults to 1000000000 and RUNS to 10. The optional arguments exist
//! only for quick sanity runs with smaller counts.

use std::env;
use std::fs::{self, File};
use std::hint::black_box;
use std::time::Instant;

use csv::Writer;
use dpds_core::delayer::{Delayer, RVDistribution};
use indicatif::{ProgressBar, ProgressStyle};

const RESULT_DIR: &str = "results";
const CSV_OUTPUT_PATH: &str = "benchmark_sampling.csv";

const MEAN_NS: f64 = 10_000_000.; // 10 ms mean delay
const STD_NS: f64 = 3_000_000.; //  3 ms jitter

const DEFAULT_N_SAMPLES: u64 = 1_000_000_000; // fixed 1B samples per experiment
const DEFAULT_RUNS: u32 = 10; // repeat each experiment for statistical significance

const DISTRIBUTIONS: [(&str, RVDistribution); 4] = [
    ("Normal", RVDistribution::NormalDistribution),
    ("Uniform", RVDistribution::UniformDistribution),
    ("Gamma", RVDistribution::GammaDistribution),
    ("LogNormal", RVDistribution::LogNormalDistribution),
];

fn main() {
    let mut args = env::args().skip(1);
    let n_samples = args
        .next()
        .map(|a| {
            a.replace('_', "")
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("Invalid sample count: {a}"))
        })
        .unwrap_or(DEFAULT_N_SAMPLES);
    let runs = args
        .next()
        .map(|a| {
            a.parse::<u32>()
                .unwrap_or_else(|_| panic!("Invalid run count: {a}"))
        })
        .unwrap_or(DEFAULT_RUNS);

    fs::create_dir_all(RESULT_DIR).expect("Failed to create results directory");
    let file = File::create(format!("{RESULT_DIR}/{CSV_OUTPUT_PATH}"))
        .expect("Failed to create benchmark CSV file");
    let mut writer = Writer::from_writer(file);
    writer
        .write_record([
            "distribution",
            "run",
            "mean_ns",
            "std_ns",
            "n_samples",
            "wall_time_s",
            "samples_per_second",
            "ns_per_sample",
        ])
        .expect("Failed to write CSV header");

    let total_runs = DISTRIBUTIONS.len() as u64 * runs as u64;
    let pb = ProgressBar::new(total_runs);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    for (name, distribution) in DISTRIBUTIONS {
        for run in 0..runs {
            pb.set_message(format!("{name} run {}/{}", run + 1, runs));

            // Correlation off: rate is irrelevant to generation speed, so pass 0.
            let mut delayer = Delayer::new(distribution, MEAN_NS, STD_NS, false, 0., 0.);

            // Sum the samples and black_box them so the loop cannot be optimized away.
            let start = Instant::now();
            let mut acc = 0.0_f64;
            for _ in 0..n_samples {
                acc += delayer.delayer_sample_ns(0.);
            }
            let elapsed = start.elapsed();
            black_box(acc);

            let wall_time_s = elapsed.as_secs_f64();
            let samples_per_second = n_samples as f64 / wall_time_s;
            let ns_per_sample = elapsed.as_nanos() as f64 / n_samples as f64;

            writer
                .write_record([
                    name,
                    &run.to_string(),
                    &MEAN_NS.to_string(),
                    &STD_NS.to_string(),
                    &n_samples.to_string(),
                    &format!("{wall_time_s:.6}"),
                    &format!("{samples_per_second:.3}"),
                    &format!("{ns_per_sample:.6}"),
                ])
                .expect("Failed to write benchmark record");
            writer.flush().expect("Failed to flush benchmark record");

            pb.inc(1);
        }
    }

    pb.finish_with_message(format!(
        "Benchmark complete -> {RESULT_DIR}/{CSV_OUTPUT_PATH}"
    ));
}
