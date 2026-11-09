//! Library-level tests of the sweep checkpoint (spec v11): JSON shape,
//! round trip, atomic write, strict loader, fingerprint.
//!
//! The binary-level behaviour (per-run saves, resume, prompts, signals, ...)
//! is tested in `checkpoint_cli.rs`; both files share the API fixed here.
//!
//! # Test-facing API fixed by these tests
//!
//! ```text
//! lib::checkpoint                      // new module, `pub mod checkpoint;` in lib.rs
//!   pub const FORMAT: &str = "dpds-simulation-checkpoint";
//!   pub const VERSION: u32 = 1;
//!   pub const FILE_NAME: &str = ".checkpoint.json";   // lives in `results/`
//!
//!   #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
//!   pub struct Checkpoint {
//!       pub format: String,                      // FORMAT
//!       pub version: u32,                        // VERSION
//!       pub fingerprint: String,                 // fingerprint(configs)
//!       pub updated_unix_s: u64,                 // time of the last save
//!       pub configs: BTreeMap<String, ConfigProgress>,  // key = csv_output_path
//!   }
//!   #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
//!   pub struct ConfigProgress {
//!       pub runs: u32,                           // simulation_config.runs
//!       pub rows: BTreeMap<u32, Vec<String>>,    // completed run index -> its
//!                                                // CSV row (the CSV_HEADER.len() strings of
//!                                                // SimulationResults::csv_record)
//!   }
//!   impl Checkpoint {
//!       pub fn new(configs: &[Config]) -> Checkpoint;    // nothing completed yet,
//!                                                        // one entry per config
//!       pub fn record(&mut self, csv_output_path: &str, run: u32, row: Vec<String>);
//!       pub fn completed_runs(&self, csv_output_path: &str) -> BTreeSet<u32>;
//!   }
//!   pub fn fingerprint(configs: &[Config]) -> String;
//!       // stable hash of the PARSED configs (every result-affecting field);
//!       // not of the file bytes; identical across processes
//!   pub fn save(path: &Path, checkpoint: &Checkpoint) -> io::Result<()>;
//!       // atomic: tmp file in the SAME directory, fsync, rename over `path`
//!   pub fn save_with(path: &Path, checkpoint: &Checkpoint,
//!                    before_rename: impl FnOnce(&Path) -> io::Result<()>) -> io::Result<()>;
//!       // `save` = `save_with(.., |_| Ok(()))`. `before_rename` gets the tmp
//!       // path after it is completely written and fsynced; if it returns
//!       // Err, `save_with` returns that Err WITHOUT renaming (the test seam
//!       // for "crash between write and rename"); the tmp file may remain.
//!   pub fn load(path: &Path) -> Result<Checkpoint, String>;
//!       // STRICT. Err(message) naming `path` for: unreadable, not valid JSON,
//!       // wrong `format`, `version != VERSION`, a run index >= that config's
//!       // `runs`, a row that does not have exactly CSV_HEADER.len() fields. Never modifies
//!       // the file. Accepts everything `save` can write (round trip).
//!       // The fingerprint is NOT checked here (the binary compares it with
//!       // the current configs).
//! ```
//!
//! JSON shape of the file (pinned by `saved_json_has_the_documented_shape`):
//!
//! ```json
//! {"format": "dpds-simulation-checkpoint", "version": 1,
//!  "fingerprint": "<opaque string>", "updated_unix_s": 1700000000,
//!  "configs": {"<csv_output_path>": {"runs": 6, "rows": {"0": ["0", "..."], "3": ["3", "..."]}}}}
//! ```

use lib::checkpoint::{self, Checkpoint, ConfigProgress, fingerprint};
use lib::config::Config;
use lib::results::CSV_HEADER;
use lib::simulation::run_simulation;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ fixtures

/// Scratch directory removed on drop (also when a test fails).
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let path = std::env::temp_dir()
            .join("dpds-simulation-tests")
            .join(format!("ckpt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Dir(path)
    }
    fn file(&self) -> PathBuf {
        self.0.join(checkpoint::FILE_NAME)
    }
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config_json(csv: &str, runs: u32, seed: u64) -> Value {
    json!({
        "simulation_config": {
            "duration_s": 0.0004, "runs": runs, "rate_gbps": 1.2,
            "average_frame_size_byte": 1480, "activate_reordering": true,
            "csv_output_path": csv, "seed": seed
        },
        "delay_config": {
            "mean_ns": 100000, "std_ns": 30000, "rv_distribution": "NormalDistribution",
            "activate_correlation": false, "half_life_period_ns": 0
        },
        "limiter_config": {"mode": "Spacer", "rate_bps": 800000000}
    })
}

fn parse(values: Vec<Value>) -> Vec<Config> {
    serde_json::from_value(Value::Array(values)).expect("configs parse")
}

fn two_configs() -> Vec<Config> {
    parse(vec![
        config_json("a.csv", 6, 100),
        config_json("b.csv", 4, 200),
    ])
}

fn row(run: u32) -> Vec<String> {
    let mut row: Vec<String> = (1..CSV_HEADER.len()).map(|i| format!("{i}.5")).collect();
    row.insert(0, run.to_string());
    row
}

fn set(runs: &[u32]) -> BTreeSet<u32> {
    runs.iter().copied().collect()
}

fn checkpoint_with(configs: &[Config], completed: &[(&str, &[u32])]) -> Checkpoint {
    let mut cp = Checkpoint::new(configs);
    for (csv, runs) in completed {
        for &run in *runs {
            cp.record(csv, run, row(run));
        }
    }
    cp
}

/// Writes `value` as the checkpoint file and returns its bytes.
fn write_json(dir: &Dir, value: &Value) -> Vec<u8> {
    let bytes = serde_json::to_vec_pretty(value).unwrap();
    fs::write(dir.file(), &bytes).unwrap();
    bytes
}

/// The JSON a valid file of `two_configs()` has, to be mutated by tests.
fn valid_json() -> Value {
    let configs = two_configs();
    let cp = checkpoint_with(&configs, &[("a.csv", &[0, 2]), ("b.csv", &[1])]);
    serde_json::to_value(&cp).unwrap()
}

// --------------------------------------------------------------------- shape

#[test]
fn new_checkpoint_has_format_version_fingerprint_and_nothing_completed() {
    let configs = two_configs();
    let cp = Checkpoint::new(&configs);
    assert_eq!(cp.format, "dpds-simulation-checkpoint");
    assert_eq!(cp.format, checkpoint::FORMAT);
    assert_eq!(cp.version, 1);
    assert_eq!(cp.version, checkpoint::VERSION);
    assert_eq!(cp.fingerprint, fingerprint(&configs));
    assert_eq!(checkpoint::FILE_NAME, ".checkpoint.json");
    assert_eq!(
        cp.configs.keys().cloned().collect::<Vec<_>>(),
        ["a.csv", "b.csv"]
    );
    assert_eq!(cp.configs["a.csv"].runs, 6);
    assert_eq!(cp.configs["b.csv"].runs, 4);
    assert!(cp.configs.values().all(|c| c.rows.is_empty()));
    assert!(cp.completed_runs("a.csv").is_empty());
}

#[test]
fn record_collects_the_set_of_completed_run_indices() {
    let configs = two_configs();
    let mut cp = Checkpoint::new(&configs);
    // completion order is not index order
    for run in [4, 0, 2] {
        cp.record("a.csv", run, row(run));
    }
    cp.record("b.csv", 3, row(3));
    assert_eq!(cp.completed_runs("a.csv"), set(&[0, 2, 4]));
    assert_eq!(cp.completed_runs("b.csv"), set(&[3]));
    assert_eq!(cp.configs["a.csv"].rows[&2], row(2));
}

#[test]
fn saved_json_has_the_documented_shape() {
    let dir = Dir::new("shape");
    let configs = two_configs();
    let cp = checkpoint_with(&configs, &[("a.csv", &[0, 3]), ("b.csv", &[])]);
    checkpoint::save(&dir.file(), &cp).unwrap();

    let value: Value = serde_json::from_slice(&fs::read(dir.file()).unwrap()).unwrap();
    let top = value.as_object().unwrap();
    let mut keys: Vec<&str> = top.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "configs",
            "fingerprint",
            "format",
            "updated_unix_s",
            "version"
        ]
    );
    assert_eq!(value["format"], "dpds-simulation-checkpoint");
    assert_eq!(value["version"], 1);
    assert!(value["fingerprint"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(value["updated_unix_s"].is_u64());

    let a = &value["configs"]["a.csv"];
    assert_eq!(a["runs"], 6);
    let rows = a["rows"].as_object().unwrap();
    assert_eq!(rows.keys().cloned().collect::<Vec<_>>(), ["0", "3"]);
    assert_eq!(rows["3"], json!(row(3)));
    assert_eq!(rows["3"].as_array().unwrap().len(), CSV_HEADER.len());
    assert_eq!(value["configs"]["b.csv"]["runs"], 4);
    assert_eq!(value["configs"]["b.csv"]["rows"], json!({}));
}

#[test]
fn updated_unix_s_is_the_current_time() {
    let dir = Dir::new("time");
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    checkpoint::save(&dir.file(), &Checkpoint::new(&two_configs())).unwrap();
    let cp = checkpoint::load(&dir.file()).unwrap();
    assert!(
        cp.updated_unix_s >= before.saturating_sub(2) && cp.updated_unix_s <= before + 60,
        "{} vs {before}",
        cp.updated_unix_s
    );
}

// ---------------------------------------------------------------- round trip

fn assert_round_trip(name: &str, cp: &Checkpoint) {
    let dir = Dir::new(name);
    checkpoint::save(&dir.file(), cp).unwrap();
    let loaded = checkpoint::load(&dir.file())
        .unwrap_or_else(|e| panic!("the loader must accept what the writer wrote ({name}): {e}"));
    assert_eq!(cp.format, loaded.format, "{name}");
    assert_eq!(cp.version, loaded.version, "{name}");
    assert_eq!(cp.fingerprint, loaded.fingerprint, "{name}");
    assert_eq!(cp.configs, loaded.configs, "{name}");
}

#[test]
fn round_trip_of_every_state_the_writer_can_be_in() {
    let configs = two_configs();
    // nothing completed (first save of a sweep)
    assert_round_trip("empty", &Checkpoint::new(&configs));
    // a prefix
    assert_round_trip(
        "prefix",
        &checkpoint_with(&configs, &[("a.csv", &[0, 1, 2])]),
    );
    // non-contiguous set, different per config
    assert_round_trip(
        "gaps",
        &checkpoint_with(&configs, &[("a.csv", &[0, 2, 5]), ("b.csv", &[1])]),
    );
    // highest valid index only
    assert_round_trip("highest", &checkpoint_with(&configs, &[("a.csv", &[5])]));
    // one run short of complete
    assert_round_trip(
        "one-short",
        &checkpoint_with(&configs, &[("a.csv", &[0, 1, 2, 3, 4])]),
    );
    // one config complete (a save between two runs), the other not started
    assert_round_trip(
        "one-complete",
        &checkpoint_with(&configs, &[("b.csv", &[0, 1, 2, 3])]),
    );
    // every run of every config done (a save right before the final cleanup)
    assert_round_trip(
        "all-complete",
        &checkpoint_with(
            &configs,
            &[("a.csv", &[0, 1, 2, 3, 4, 5]), ("b.csv", &[0, 1, 2, 3])],
        ),
    );
}

#[test]
fn round_trip_with_a_config_without_runs() {
    let configs = parse(vec![
        config_json("none.csv", 0, 1),
        config_json("one.csv", 1, 2),
    ]);
    assert_round_trip("zero-runs", &Checkpoint::new(&configs));
    assert_round_trip(
        "zero-runs-with-progress",
        &checkpoint_with(&configs, &[("one.csv", &[0])]),
    );
}

#[test]
fn round_trip_with_the_real_rows_including_nan_and_infinite_values() {
    // Total loss -> no packet is tracked: the CSV row contains values such as
    // NaN. The checkpoint stores rows as strings, so it must take them all.
    let mut value = config_json("nan.csv", 2, 7);
    value["simulation_config"]["loss_model"] = json!({"Random": 1.0});
    let configs = parse(vec![value]);
    let mut cp = Checkpoint::new(&configs);
    for run in 0..2 {
        let row = run_simulation(&configs[0], run).csv_record(run).to_vec();
        assert_eq!(row.len(), CSV_HEADER.len());
        cp.record("nan.csv", run, row);
    }
    assert_round_trip("real-rows", &cp);
}

#[test]
fn round_trip_after_a_second_save_over_the_first() {
    let dir = Dir::new("resave");
    let configs = two_configs();
    let first = checkpoint_with(&configs, &[("a.csv", &[0])]);
    let second = checkpoint_with(&configs, &[("a.csv", &[0, 1]), ("b.csv", &[2])]);
    checkpoint::save(&dir.file(), &first).unwrap();
    checkpoint::save(&dir.file(), &second).unwrap();
    assert_eq!(
        checkpoint::load(&dir.file()).unwrap().configs,
        second.configs
    );
}

#[test]
fn save_leaves_nothing_but_the_checkpoint_in_the_directory() {
    let dir = Dir::new("no-leftovers");
    let configs = two_configs();
    for runs in [&[0u32][..], &[0, 1], &[0, 1, 2]] {
        checkpoint::save(&dir.file(), &checkpoint_with(&configs, &[("a.csv", runs)])).unwrap();
        assert_eq!(dir.entries(), [".checkpoint.json"]);
    }
}

#[test]
fn loader_accepts_hand_edited_files() {
    // Rows of a pretty-printed file in any key order; a run removed by hand
    // (the documented way to rerun a single run).
    let dir = Dir::new("hand-edit");
    let mut value = valid_json();
    value["configs"]["a.csv"]["rows"]
        .as_object_mut()
        .unwrap()
        .remove("0");
    write_json(&dir, &value);
    let cp = checkpoint::load(&dir.file()).unwrap();
    assert_eq!(cp.completed_runs("a.csv"), set(&[2]));
    assert_eq!(cp.completed_runs("b.csv"), set(&[1]));
}

// ------------------------------------------------------------- atomic write

#[test]
fn crash_between_write_and_rename_keeps_the_previous_checkpoint_intact() {
    let dir = Dir::new("atomic");
    let configs = two_configs();
    let old = checkpoint_with(&configs, &[("a.csv", &[0])]);
    let new = checkpoint_with(&configs, &[("a.csv", &[0, 1]), ("b.csv", &[0])]);
    checkpoint::save(&dir.file(), &old).unwrap();
    let old_bytes = fs::read(dir.file()).unwrap();

    let mut tmp_seen: Option<PathBuf> = None;
    let result = checkpoint::save_with(&dir.file(), &new, |tmp| {
        tmp_seen = Some(tmp.to_path_buf());
        // simulated crash: tmp is complete, the rename never happens
        Err(io::Error::other("simulated crash"))
    });
    assert!(result.is_err(), "a failing hook must surface as an error");

    // the visible checkpoint is exactly the old one (not partial, not new)
    assert_eq!(fs::read(dir.file()).unwrap(), old_bytes);
    assert_eq!(
        checkpoint::load(&dir.file()).unwrap().configs,
        old.configs,
        "old checkpoint must still load"
    );

    // the tmp file was in the same directory (rename atomicity needs that),
    // was not the checkpoint itself and was completely written
    let tmp = tmp_seen.expect("hook must be called with the tmp path");
    assert_eq!(tmp.parent(), dir.file().parent());
    assert_ne!(tmp, dir.file());
    let written: Checkpoint =
        serde_json::from_slice(&fs::read(&tmp).expect("tmp file exists when the hook runs"))
            .expect("tmp file must hold complete JSON before the rename");
    assert_eq!(written.configs, new.configs);

    // a later normal save is unaffected by the leftover
    checkpoint::save(&dir.file(), &new).unwrap();
    assert_eq!(checkpoint::load(&dir.file()).unwrap().configs, new.configs);
}

#[test]
fn crash_during_the_very_first_save_leaves_no_checkpoint() {
    let dir = Dir::new("atomic-first");
    let cp = Checkpoint::new(&two_configs());
    let result = checkpoint::save_with(&dir.file(), &cp, |_| Err(io::Error::other("crash")));
    assert!(result.is_err());
    assert!(
        !dir.file().exists(),
        "no (partial) checkpoint may be visible"
    );
}

#[test]
fn hook_runs_after_the_data_is_written_and_a_successful_hook_renames() {
    let dir = Dir::new("hook-ok");
    let cp = checkpoint_with(&two_configs(), &[("a.csv", &[1])]);
    let mut called = 0;
    checkpoint::save_with(&dir.file(), &cp, |tmp| {
        called += 1;
        assert!(!dir.file().exists(), "rename must come after the hook");
        assert!(tmp.exists());
        Ok(())
    })
    .unwrap();
    assert_eq!(called, 1);
    assert_eq!(checkpoint::load(&dir.file()).unwrap().configs, cp.configs);
}

// ------------------------------------------------------------- strict loader

/// `load` must fail with a message naming the file and must not touch it.
fn assert_rejected(name: &str, write: impl FnOnce(&Dir) -> Vec<u8>) {
    let dir = Dir::new(name);
    let bytes = write(&dir);
    let err = checkpoint::load(&dir.file()).expect_err(name);
    assert!(
        err.contains(".checkpoint.json"),
        "{name}: message must name the file: {err}"
    );
    assert_eq!(fs::read(dir.file()).unwrap(), bytes, "{name}: file changed");
}

fn rejected_mutation(name: &str, mutate: impl FnOnce(&mut Value)) {
    assert_rejected(name, |dir| {
        let mut value = valid_json();
        mutate(&mut value);
        write_json(dir, &value)
    });
}

#[test]
fn loader_rejects_unparsable_files_naming_the_path() {
    assert_rejected("garbage", |dir| {
        let bytes = b"this is not json {".to_vec();
        fs::write(dir.file(), &bytes).unwrap();
        bytes
    });
    assert_rejected("empty-file", |dir| {
        fs::write(dir.file(), b"").unwrap();
        Vec::new()
    });
    assert_rejected("truncated", |dir| {
        let full = serde_json::to_vec_pretty(&valid_json()).unwrap();
        let bytes = full[..full.len() / 2].to_vec();
        fs::write(dir.file(), &bytes).unwrap();
        bytes
    });
    assert_rejected("json-but-not-an-object", |dir| {
        fs::write(dir.file(), b"[1, 2, 3]").unwrap();
        b"[1, 2, 3]".to_vec()
    });
}

#[test]
fn loader_rejects_a_missing_file_naming_the_path() {
    let dir = Dir::new("missing");
    let err = checkpoint::load(&dir.file()).expect_err("no file");
    assert!(err.contains(".checkpoint.json"), "{err}");
}

#[test]
fn loader_rejects_missing_fields() {
    for field in ["format", "version", "fingerprint", "configs"] {
        rejected_mutation(&format!("missing-{field}"), |v| {
            v.as_object_mut().unwrap().remove(field);
        });
    }
}

#[test]
fn loader_rejects_another_format_or_version() {
    rejected_mutation("other-format", |v| v["format"] = json!("something-else"));
    rejected_mutation("empty-format", |v| v["format"] = json!(""));
    for version in [0, 2, 3, 99] {
        rejected_mutation(&format!("version-{version}"), |v| {
            v["version"] = json!(version);
        });
    }
    rejected_mutation("version-string", |v| v["version"] = json!("1"));
}

#[test]
fn loader_rejects_run_indices_at_or_above_the_run_count() {
    // a.csv has 6 runs: index 6 is out of range, 5 is the highest valid one
    rejected_mutation("index-at-runs", |v| {
        v["configs"]["a.csv"]["rows"]["6"] = json!(row(6));
    });
    rejected_mutation("index-far-above", |v| {
        v["configs"]["b.csv"]["rows"]["400"] = json!(row(0));
    });
    rejected_mutation("negative-index", |v| {
        v["configs"]["a.csv"]["rows"]["-1"] = json!(row(0));
    });
    rejected_mutation("float-index", |v| {
        v["configs"]["a.csv"]["rows"]["1.5"] = json!(row(1));
    });
    rejected_mutation("non-numeric-index", |v| {
        v["configs"]["a.csv"]["rows"]["x"] = json!(row(1));
    });
    // boundary: the highest valid index loads
    let dir = Dir::new("index-highest-ok");
    let mut value = valid_json();
    value["configs"]["a.csv"]["rows"]["5"] = json!(row(5));
    write_json(&dir, &value);
    assert!(
        checkpoint::load(&dir.file()).unwrap().configs["a.csv"]
            .rows
            .contains_key(&5)
    );
}

#[test]
fn loader_rejects_rows_whose_first_field_is_not_their_run_index() {
    // field 0 of a row is its `run` column: a row filed under another index
    // (or with a non-numeric run) would put a wrong CSV together on resume
    rejected_mutation("row-of-another-run", |v| {
        v["configs"]["a.csv"]["rows"]["0"] = json!(row(1));
    });
    rejected_mutation("row-of-run-zero-under-2", |v| {
        v["configs"]["a.csv"]["rows"]["2"] = json!(row(0));
    });
    rejected_mutation("row-of-run-above-the-runs", |v| {
        v["configs"]["b.csv"]["rows"]["1"] = json!(row(40));
    });
    for (name, field) in [("empty", ""), ("text", "one")] {
        rejected_mutation(&format!("first-field-{name}"), |v| {
            let mut r = row(1);
            r[0] = field.to_string();
            v["configs"]["b.csv"]["rows"]["1"] = json!(r);
        });
    }
}

#[test]
fn loader_rejects_rows_that_do_not_have_one_field_per_csv_column() {
    rejected_mutation("row-too-short", |v| {
        v["configs"]["a.csv"]["rows"]["0"] = json!(["0", "1"]);
    });
    rejected_mutation("row-too-long", |v| {
        let mut r = row(0);
        r.push("extra".into());
        v["configs"]["a.csv"]["rows"]["0"] = json!(r);
    });
    rejected_mutation("row-empty", |v| {
        v["configs"]["a.csv"]["rows"]["0"] = json!([]);
    });
    rejected_mutation("row-not-an-array", |v| {
        v["configs"]["a.csv"]["rows"]["0"] = json!("0,1,2");
    });
}

#[test]
fn loader_does_not_modify_files_it_accepts_either() {
    let dir = Dir::new("read-only");
    let bytes = write_json(&dir, &valid_json());
    checkpoint::load(&dir.file()).unwrap();
    assert_eq!(fs::read(dir.file()).unwrap(), bytes);
    assert_eq!(dir.entries(), [".checkpoint.json"]);
}

// --------------------------------------------------------------- fingerprint

fn fp(value: Value) -> String {
    fingerprint(&parse(vec![value]))
}

fn base() -> Value {
    config_json("fp.csv", 5, 11)
}

#[test]
fn fingerprint_is_deterministic_and_non_empty() {
    let configs = two_configs();
    assert!(!fingerprint(&configs).is_empty());
    assert_eq!(fingerprint(&configs), fingerprint(&two_configs()));
    assert_ne!(fingerprint(&configs), fingerprint(&configs[..1]));
}

#[test]
fn fingerprint_depends_on_the_parsed_content_not_on_the_file_bytes() {
    let compact = r#"[{"simulation_config":{"duration_s":0.001,"runs":3,"rate_gbps":10,
        "average_frame_size_byte":1518,"activate_reordering":false,"csv_output_path":"x.csv",
        "seed":9},"delay_config":{"mean_ns":1e7,"std_ns":100000,"rv_distribution":"NormalDistribution",
        "activate_correlation":false,"half_life_period_ns":0}}]"#;
    // other whitespace, other key order, other number spelling
    let spaced = r#"
    [
      {
        "delay_config": {
            "half_life_period_ns": 0.0, "activate_correlation": false,
            "rv_distribution": "NormalDistribution", "std_ns": 1.0e5, "mean_ns": 10000000
        },
        "simulation_config": {
            "seed": 9,
            "csv_output_path": "x.csv",
            "activate_reordering": false,
            "average_frame_size_byte": 1518.0,
            "rate_gbps": 10.0,
            "runs": 3,
            "duration_s": 1e-3
        }
      }
    ]"#;
    let a: Vec<Config> = serde_json::from_str(compact).unwrap();
    let b: Vec<Config> = serde_json::from_str(spaced).unwrap();
    assert_eq!(fingerprint(&a), fingerprint(&b));
}

#[test]
fn fingerprint_changes_with_every_field_that_affects_results() {
    let reference = fp(base());
    let mut changed: Vec<(&str, Value)> = Vec::new();
    let mut variant = |name: &'static str, edit: &dyn Fn(&mut Value)| {
        let mut value = base();
        edit(&mut value);
        changed.push((name, value));
    };
    variant("duration_s", &|v| {
        v["simulation_config"]["duration_s"] = json!(0.0005)
    });
    variant("rate_gbps", &|v| {
        v["simulation_config"]["rate_gbps"] = json!(1.3)
    });
    variant("average_frame_size_byte", &|v| {
        v["simulation_config"]["average_frame_size_byte"] = json!(1481)
    });
    variant("activate_reordering", &|v| {
        v["simulation_config"]["activate_reordering"] = json!(false)
    });
    variant("reordering_metric", &|v| {
        v["simulation_config"]["reordering_metric"] = json!("kendall_tau_distance")
    });
    // also the limiter's limit: dpds-core derives it from the capacity
    variant("buffer_capacity", &|v| {
        v["simulation_config"]["buffer_capacity"] = json!(77)
    });
    variant("loss_model", &|v| {
        v["simulation_config"]["loss_model"] = json!({"Random": 0.01})
    });
    variant("seed", &|v| v["simulation_config"]["seed"] = json!(12));
    variant("seed removed", &|v| {
        v["simulation_config"]
            .as_object_mut()
            .unwrap()
            .remove("seed");
    });
    variant("csv_output_path", &|v| {
        v["simulation_config"]["csv_output_path"] = json!("other.csv")
    });
    variant("delay mean_ns", &|v| {
        v["delay_config"]["mean_ns"] = json!(100001)
    });
    variant("delay std_ns", &|v| {
        v["delay_config"]["std_ns"] = json!(30001)
    });
    variant("delay distribution", &|v| {
        v["delay_config"]["rv_distribution"] = json!("UniformDistribution")
    });
    variant("delay correlation", &|v| {
        v["delay_config"]["activate_correlation"] = json!(true)
    });
    variant("delay half life", &|v| {
        v["delay_config"]["half_life_period_ns"] = json!(5)
    });
    variant("delay removed", &|v| {
        v.as_object_mut().unwrap().remove("delay_config");
    });
    variant("limiter mode", &|v| {
        v["limiter_config"]["mode"] = json!("TokenBucketFilter")
    });
    variant("limiter rate", &|v| {
        v["limiter_config"]["rate_bps"] = json!(800000001)
    });
    variant("limiter removed", &|v| {
        v.as_object_mut().unwrap().remove("limiter_config");
    });

    let mut seen = BTreeSet::new();
    for (name, value) in changed {
        let f = fp(value);
        assert_ne!(f, reference, "changing {name} must change the fingerprint");
        assert!(seen.insert(f), "{name}: collides with another variant");
    }
}

#[test]
fn fingerprint_depends_on_the_set_of_configs() {
    let a = config_json("a.csv", 3, 1);
    let b = config_json("b.csv", 3, 2);
    let both = fingerprint(&parse(vec![a.clone(), b.clone()]));
    assert_ne!(both, fingerprint(&parse(vec![a.clone()])));
    assert_ne!(both, fingerprint(&parse(vec![b])));
    assert_ne!(
        fingerprint(&parse(vec![a.clone()])),
        fingerprint(&parse(vec![a.clone(), a]))
    );
}

#[test]
fn config_progress_is_a_plain_data_struct() {
    // pins the public fields used by the binary and by the other test file
    let progress = ConfigProgress {
        runs: 2,
        rows: [(1, row(1))].into_iter().collect(),
    };
    assert_eq!(progress.rows.len(), 1);
    assert_eq!(
        Path::new(checkpoint::FILE_NAME).extension().unwrap(),
        "json"
    );
}
