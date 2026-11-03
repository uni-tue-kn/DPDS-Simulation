# dpds-simulation

A discrete-event simulation of a network-link emulator. It models packets
flowing through a configurable **delayer** (variable latency / jitter, optionally
correlation, drawn from several distributions) and a **rate limiter**, then
measures the statistics that characterise the emulated link: one-way delay,
inter-arrival time (jitter), packet reordering, delay *passage times*, burst
spacing, and packet drops.

It is the simulation companion to the
[`dpds-core`](https://github.com/uni-tue-kn/DPDS/tree/nightly/crates/dpds-core) crate
(part of [DPDS](https://github.com/uni-tue-kn/DPDS)), which provides the underlying
delayer and limiter models. The simulation sweeps a large grid of link
configurations, writes per-run results to CSV, and ships R scripts that turn
those CSVs into the plots used for analysis.

## Requirements

- **Rust** 1.99 or newer (the toolchain `dpds-core` is developed and tested with).
- Network access to fetch the `dpds-core` git dependency at build time
  (see [Dependencies](#dependencies)).
- **R** with `ggplot2`, `dplyr`, `tidyr`, `magrittr`, `scales`, and `patchwork`.
  — Only needed to (re)generate the plots in `results/`.

## Building

```sh
cargo build --release
```

This builds three binaries:

| Binary            | Purpose                                                                        |
| ----------------- | ------------------------------------------------------------------------------ |
| `dpds_simulation` | Runs the full configuration sweep from `simulation_configs.json`.              |
| `dpds_benchmark`  | Standalone wall-clock throughput benchmark of the delay-distribution sampling. |

## Running a simulation

The simulation reads its workload from `simulation_configs.json` in the working
directory and writes one CSV per configuration into `results/`:

```sh
cargo run --release --bin dpds-simulation
```

Runs are executed concurrently. Cap the number of simultaneous runs with the
`MAX_CONCURRENT_SIMULATIONS` environment variable (default `2`):

```sh
MAX_CONCURRENT_SIMULATIONS=8 cargo run --release --bin dpds-simulation
```

Each output CSV contains one row per run with the following columns:

```
run, tracked_packets,
inter_arrival_time_mean_ns, inter_arrival_time_std_ns,
delay_mean_ns, delay_std_ns,
kendall_tau_distance, kendall_tau_distance_div_n,
out_of_order_packets, out_of_order_packets_div_n,
tracked_theoretical_passage_times, theoretical_passage_time_mean_ns, theoretical_passage_time_std_ns,
tracked_measured_passage_times, measured_passage_time_mean_ns, measured_passage_time_std_ns,
dropped_packets_bandwidth,
burst_spacing_offset_mean_ns, burst_spacing_offset_std_ns, burst_spacing_offset_max_ns,
dropped_packets_burst_virtual_queue
```

## Running the benchmark

`dpds-benchmark` measures how fast each delay distribution can be sampled,
independent of the event simulation, and writes `results/benchmark_sampling.csv`:

```sh
# Defaults: 1,000,000,000 samples per distribution, 10 repetitions.
cargo run --release --bin dpds-benchmark

# Quick sanity run: fewer samples and repetitions.
cargo run --release --bin dpds-benchmark 1_000_000 2
```

## Configuration format

`simulation_configs.json` is a JSON array of configuration objects. Each object
has a required `simulation_config` and optional `delay_config` and
`limiter_config`:

```jsonc
{
  "simulation_config": {
    "duration_s": 600,                 // simulated time per run
    "runs": 10,                        // independent repetitions
    "rate_gbps": 0.1,                  // offered load
    "average_frame_size_byte": 1518,   // mean frame size (a 20-byte overhead is added internally)
    "activate_reordering": false,      // allow packets to be emitted out of order
    "csv_output_path": "basic_normal_10ms_100us_0.1Gbps.csv",
    "reordering_metric": "out_of_order_counter" // or "kendall_tau_distance" (optional, default: out_of_order_counter)
  },
  "delay_config": {                    // optional; omit for a zero-delay link
    "mean_ns": 10000000,               // 10 ms mean delay
    "std_ns": 100000,                  // 100 us jitter (std)
    "rv_distribution": "NormalDistribution", // Normal | Uniform | Gamma | LogNormal | Deterministic
    "activate_correlation": false,     // auto-correlate successive delays
    "half_life_period_ns": 0           // correlation half-life (used when correlation is on)
  },
  "limiter_config": {                  // optional; omit for an unlimited link
    "mode": "Spacer",
    "rate_bps": 1000000000,
    "limit_bit": 1000000
  }
}
```

The committed `simulation_configs.json` encodes the full experiment grid used for
the analysis (316 configurations sweeping distribution, delay, jitter, rate,
correlation, and limiter settings).

## Generating plots

Simulation outputs (CSV/PDF) are intentionally **not** committed — only the
analysis scripts are. After a run, regenerate every figure with:

```sh
cd results
make            # run all R scripts sequentially
make clean      # remove generated PDFs and aggregated CSVs
```

See `results/Makefile` for individual targets.

## Dependencies

This crate depends on the `dpds-core` crate of the
[DPDS](https://github.com/uni-tue-kn/DPDS) repository, fetched as a git
dependency on its `nightly` branch (Cargo finds the package inside the DPDS
workspace by name):

```toml
dpds-core = { git = "https://github.com/uni-tue-kn/DPDS", branch = "nightly" }
```

The committed `Cargo.lock` pins the exact revision used, so builds are
reproducible. To update to a newer `dpds-core`, run `cargo update -p dpds-core`.
The simulation only uses `dpds-core`'s `Delayer`, `Limiter`, `RVDistribution`
and `LimiterMode` (it does not use `Buffer`, `Dropper` or `Config`). It relies
on the serde derives of `RVDistribution` and `LimiterMode`, which come from
`dpds-core`'s default `json` feature (implies `serde`).

### Building against a local DPDS checkout

To build against a local DPDS working tree (for example to try unpushed
changes) without editing `Cargo.toml`, override the dependency on the command
line:

```sh
cargo test --config 'patch."https://github.com/uni-tue-kn/DPDS".dpds-core.path="../DPDS/crates/dpds-core"'
```

Use the same `--config` flag for `cargo build`, `cargo run`, `cargo clippy`,
and so on. Cargo rewrites `Cargo.lock` while the patch is active (the
`dpds-core` entry loses its git `source`). Do not commit that; afterwards
restore the lockfile with `git checkout Cargo.lock` (or run a plain `cargo
build` without the flag).

### Behaviour of the `dpds-core` model

Compared with the former `DPDS-Core` crate, the delayer in `dpds-core` changed
in ways that alter the simulation results for correlated delays:

- **Start-up fix.** The correlated delay spread is now derived from the
  correlation weight from the very first packet. Previously the first
  roughly 10 ms of every run sampled with an almost vanishing delay spread.
- **Rate update period.** The packet-rate estimate that adapts the correlation
  weight is refreshed every 1 ms instead of every 10 ms. At low offered loads
  (about 8 packets per ms at 0.1 Gbit/s) this estimate is coarser than before.

The limiter (`Spacer` and `TokenBucketFilter`) is unchanged.

## Tests

```sh
cargo test
```

The test suite validates the reordering metrics — including a brute-force
cross-check of the online Kendall tau distance against a naive inversion count
under randomised reordering.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
