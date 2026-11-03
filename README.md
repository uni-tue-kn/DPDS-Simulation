# dpds-simulation

A discrete-event simulation of a network-link emulator. It models packets
flowing through a configurable **delayer** (variable latency / jitter, optionally
correlation, drawn from several distributions), a **rate limiter** and a **loss
model**, then measures the statistics that characterise the emulated link: one-way delay,
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

This builds two binaries:

| Binary            | Purpose                                                                        |
| ----------------- | ------------------------------------------------------------------------------ |
| `dpds_simulation` | Runs the full configuration sweep from `simulation_configs.json`.              |
| `dpds_benchmark`  | Standalone wall-clock throughput benchmark of the delay-distribution sampling. |

The release profile uses fat LTO, one codegen unit and `panic = "abort"`.

## Model

Each run merges two event sources:

1. the **arrival queue**: one pending arrival at a time (arrival `k` happens at
   `k * iat`, `iat = 1e9 / (rate_gbps * 1e9 / ((average_frame_size_byte + 20) * 8))` ns);
2. the real **`dpds_core::buffer::Buffer`**, built from the simulation config
   (delayer, limiter, `activate_reordering`, `buffer_capacity`, `loss_model`,
   `seed`). Every arrival is offered with `push_outcome`; the buffer applies
   delay, limiter, loss and capacity, keeps FIFO head-of-line blocking or
   reorders by send time (including its 1/64 ns tie rule), and reports
   `Accepted { send_time_ns }`, `Lost`, `LimiterDrop` or `BufferFull`.

The next event is the earlier of the next arrival and the buffer head's send
time. Packets are popped at their exact send time (no poll-loop granularity).
Rules:

- A transmit event at time `t` is processed **before** an arrival at `t`
  (a buffer slot freed at `t` is usable by the arrival at `t`).
- An arrival at exactly `t == duration` is processed; afterwards the buffer is
  drained, so every accepted packet departs.
- A packet leaves at `max(clock, send time)`: at its send time, or in FIFO mode,
  when blocked behind a later packet, right after its predecessor.
- The packet handed to the buffer is the opaque pointer `seq + 1` (never
  dereferenced); its arrival time is recomputed from `seq`.
- Theoretical passage time: the tracker is fed `send_time - rx` at the arrival
  of every accepted packet. Measured passage time and the delay histogram use
  `tx - rx` at the departure.
- Frame length passed to the buffer is `ceil(average_frame_size_byte)`;
  `dpds-core` adds 4 bytes (FEC) and the 20 bytes of L1 overhead itself, so
  the limiter sees `(ceil(frame) + 4 + 20) * 8` bit.
- The burst virtual queue is unchanged: a `Spacer` at `rate_gbps / 0.9` fed at
  each departure with `ceil(frame)` bytes (no FEC), recording the offsets.

## Running a simulation

The simulation reads its workload from `simulation_configs.json` in the working
directory and writes one CSV per configuration into `results/`:

```sh
cargo run --release --bin dpds_simulation
```

All `(config, run)` jobs of the sweep share one [rayon](https://docs.rs/rayon)
thread pool. The default size is the available parallelism; override it with the
`MAX_CONCURRENT_SIMULATIONS` environment variable (an invalid value or `0` falls
back to the default):

```sh
MAX_CONCURRENT_SIMULATIONS=8 cargo run --release --bin dpds_simulation
```

Jobs are handed out run-major: run 0 of every config, then run 1 of every
config, and so on, so an interrupted sweep has some runs of every config
instead of all runs of a few. A config's CSV is written (atomically, from the
rows of the checkpoint) once all of its runs have finished, with the rows sorted
by run index. With a `seed` set, the files are byte-identical for any worker
count and for any order or interruption of the runs.

### Options

```text
dpds_simulation [--resume | --fresh] [--dry-run] [--stop-at HH:MM] [--start-at HH:MM]
```

| Option             | Effect                                                                                                            |
| ------------------ | ----------------------------------------------------------------------------------------------------------------- |
| `--resume`         | Continue from an existing checkpoint without asking.                                                              |
| `--fresh`          | Delete an existing checkpoint (even an unusable one) and start over without asking. Exclusive with `--resume`.     |
| `--dry-run`        | Print the job plan (configs, runs, estimated simulated packets, checkpoint state) and exit; writes nothing.      |
| `--stop-at HH:MM`  | Stop handing out jobs at that local time (next occurrence), then behave like a first Ctrl+C.                      |
| `--start-at HH:MM` | Wait until that local time (next occurrence) before starting (after any checkpoint question).                         |

### Checkpoint and resume

After **every completed run** the sweep rewrites `results/.checkpoint.json`
atomically (temporary file in the same directory, fsync, rename), so even a
`kill -9` leaves a valid file. It holds a format name and version, a
fingerprint of the parsed `simulation_configs.json` (every field that affects
the results, but not its whitespace or key order; `runs` and `csv_output_path`
included), and per config (keyed by `csv_output_path`) the set of completed run
indices with their CSV rows. A resumed sweep skips exactly the recorded runs
and rebuilds the CSVs from the recorded rows. At the end of a complete sweep
the checkpoint is removed. To rerun a single run, delete its entry from the
file by hand.

When a checkpoint exists at start-up, a summary is printed (completed/total
runs, age) and:

- with `--resume` it is continued, with `--fresh` it is deleted and the sweep
  starts over;
- with neither flag on a terminal, the question `Resume it? [y/n]` is asked; `n`
  asks `Really delete the checkpoint and start fresh? [y/n]`, and a second `n`
  exits with the checkpoint untouched. Other input is asked again; end of input
  is an error;
- with neither flag and no terminal on stdin, the sweep exits with an error
  instead of guessing.

A checkpoint that cannot be parsed, has another format or version, or whose
fingerprint does not match the current configs **aborts** with a message
(naming `--fresh` as the way out); it is never replaced silently. Without a
checkpoint, `--resume` and `--fresh` simply start the sweep.

**Interruption.** The first Ctrl+C (SIGINT) or SIGTERM stops handing out jobs;
runs in flight finish and are checkpointed, the CSVs of complete configs are
written, and the process exits with a non-zero code after printing how to
resume. A second Ctrl+C exits immediately (the checkpoint is current up to the
last completed run). A panic in a run prints the config and run that failed and
exits non-zero; completed runs stay checkpointed (also with `panic = "abort"`:
the panic hook ends the process itself).

The progress bar shows completed/total runs, including the resumed ones, and an
ETA.

### Buffer capacity

Each run reserves 16 B per packet of `buffer_capacity` up front (32 MiB for
the default 2^21 = 2097152). The buffer must never be the bottleneck, so the
capacity has to exceed the number of packets in flight, about
`rate_pps * (mean_ns + a few std_ns) / 1e9` plus the limiter's queue,
which the buffer bounds as well: dpds-core derives the limiter limit as
`buffer_capacity * 1542 B * 8` (1518 B maximum frame + 4 FEC + 20 B L1 overhead)
and checks for buffer room before the delay, spacing and loss. For the committed grid the largest value is the
100 Gbit/s, 100 ms configuration: 8.13 Mpps * 100 ms = 0.81 M packets, 0.96 M
at mean + 6 std (a 0.6 s simulated run of its four jitter variants showed no
`dropped_packets_buffer_full`). 2^21 therefore holds with a margin of about a
factor of 2.2 over the worst config, not orders of magnitude: when adding
configs with a larger rate-delay product, set `buffer_capacity` explicitly and
check that `dropped_packets_buffer_full` is 0.

Each output CSV contains one row per run with the following columns:

```
run, tracked_packets,
inter_arrival_time_mean_ns, inter_arrival_time_std_ns,
delay_mean_ns, delay_std_ns,
kendall_tau_distance, kendall_tau_distance_div_n,
out_of_order_packets, out_of_order_packets_div_n,
tracked_theoretical_passage_times, theoretical_passage_time_mean_ns, theoretical_passage_time_std_ns,
tracked_measured_passage_times, measured_passage_time_mean_ns, measured_passage_time_std_ns,
dropped_packets_loss, dropped_packets_limiter, dropped_packets_buffer_full,
burst_spacing_offset_mean_ns, burst_spacing_offset_std_ns, burst_spacing_offset_max_ns,
dropped_packets_burst_virtual_queue
```

## Running the benchmark

`dpds_benchmark` measures how fast each delay distribution can be sampled,
independent of the event simulation, and writes `results/benchmark_sampling.csv`:

```sh
# Defaults: 1,000,000,000 samples per distribution, 10 repetitions.
cargo run --release --bin dpds_benchmark

# Quick sanity run: fewer samples and repetitions.
cargo run --release --bin dpds_benchmark 1_000_000 2
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
    "reordering_metric": "out_of_order_counter", // or "kendall_tau_distance" (optional, default: out_of_order_counter)
    "buffer_capacity": 2097152,        // optional, packets the dpds-core buffer holds (default: 2^21, see below)
    "loss_model": {"Random": 0.01},    // optional, default {"Random": 0.0}; or {"GilbertElliot": [p, r, k, h, true]}
    "seed": 42                         // optional; run r is seeded with seed + r (default: OS entropy, not reproducible)
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
    "rate_bps": 1000000000            // the limit is derived from buffer_capacity
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
The simulation runs the real `dpds-core` `Buffer` (with its `Delayer`, `Limiter`
and `Dropper`) and uses `Limiter` for the burst virtual queue. It relies on the
serde derives of `RVDistribution`, `LimiterMode` and `LossModel`, which come
from `dpds-core`'s default `json` feature (implies `serde`).

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

### Behaviour changes of the buffer-based simulation

Compared with the former event-queue model (see git history for the numbers
of the paper):

- The simulation's own transmit-time limiter and its two transmission event
  kinds are gone; the `Buffer` does limiting, FIFO head-of-line blocking and
  reordering.
- The limiter is applied at **push time** to the send time
  (`arrival + delay`), as in the real emulator, not at the transmit time.
  Packets the limiter rejects are really dropped; the old model still
  transmitted them (it only counted them).
- `dropped_packets_bandwidth` is replaced (in place) by
  `dropped_packets_loss`, `dropped_packets_limiter` and
  `dropped_packets_buffer_full`. If a packet is both lost and rejected by the
  limiter, it counts as lost.
- New optional config fields `buffer_capacity`, `loss_model`, `seed`.
- Results include the 4 FEC bytes `dpds-core` adds to every frame for the
  limiter (the burst virtual queue does not).
- The correlated delayer starts from the config's offered packet rate
  (`initialization_rate_pps` = 1e9 / inter-arrival time in ns, a required
  `dpds-core` field), as the old model did; it is derived, not a config field.
- Every departure is tracked; the old model skipped the first packet.
  `inter_arrival_time_*` describes the `n - 1` departure gaps.
- Theoretical passage times use the buffer's whole-ns send time minus the
  arrival time (so including truncation and limiter offset), not the raw
  sampled delay.
- Send times are truncated to whole ns by the buffer (and, in reordering mode,
  carry its 1/64 ns tie offsets).
- Arrival times are `k * iat` instead of accumulated sums (no float drift).
- Without a `seed` runs are not reproducible (as before); with one they are,
  independent of the worker count. Rows in the CSVs are sorted by run index.
- The sweep driver uses one flat rayon thread pool over all `(config, run)`
  jobs instead of tokio, defaulting to the available parallelism (previously 2
  concurrent runs), hands them out run-major and checkpoints after every run
  (see [Checkpoint and resume](#checkpoint-and-resume)).
- A transmit event at time `t` is processed before an arrival at `t` (tie
  rule), so a buffer slot freed at `t` is usable by the arrival at `t`.
- The reordering statistics are fed the rank of a departing packet among the
  **accepted** packets, not its arrival index, so lost, limited or buffer-full
  packets (gaps in the arrival index) never count as inversions of the Kendall
  tau distance.
- The default `buffer_capacity` is 2^21 packets instead of 2^24 (see
  [Buffer capacity](#buffer-capacity)).

## Tests

```sh
cargo test --all-targets
```

The suite covers:

- the reordering metrics, including a brute-force cross-check of the online
  Kendall tau distance against a naive inversion count under randomised
  reordering (`tests/statistics.rs`);
- the event model against hand-derived numbers and trace-based oracles
  (`tests/model.rs`) and the config format (`tests/config.rs`);
- the sweep driver end to end (`tests/sweep_driver.rs`): CSV shape, worker
  count independence, release profile, and static checks of the R scripts,
  dependencies and this README;
- the checkpoint file (`tests/checkpoint.rs`: JSON shape, round trip, atomic
  write, strict loader, fingerprint) and the checkpointed sweep
  (`tests/checkpoint_cli.rs`: per-run saves, resume, prompts, `--fresh`,
  `--dry-run`, run-major order, signals, panics, `--stop-at`/`--start-at`).

CI additionally runs `cargo fmt --all -- --check` and
`cargo clippy --all-targets --all-features -- -D warnings`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
