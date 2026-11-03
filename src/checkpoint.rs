//! Checkpoint of a sweep: the CSV row of every completed `(config, run)`.
//!
//! The file `results/.checkpoint.json` is rewritten atomically (temp file in
//! the same directory, fsync, rename, fsync of the directory) after every
//! completed run, so a kill at any moment leaves either the previous or the
//! new complete file. Completed runs are recorded as a set of run indices with
//! their rows (not as a count), so a resumed sweep skips exactly those runs and
//! can rebuild the CSVs without re-running anything.
//!
//! [`load`] is strict (anything it cannot use is an error, never a fresh
//! start) but accepts every state [`save`] can write. The fingerprint is
//! compared by the caller.

use crate::config::Config;
use crate::results::CSV_HEADER;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const FORMAT: &str = "dpds-simulation-checkpoint";
pub const VERSION: u32 = 1;
/// Name of the checkpoint file inside the results directory.
pub const FILE_NAME: &str = ".checkpoint.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub format: String,
    pub version: u32,
    /// [`fingerprint`] of the configs the rows belong to.
    pub fingerprint: String,
    /// Time of the last change.
    pub updated_unix_s: u64,
    /// Progress per config, keyed by `csv_output_path`.
    pub configs: BTreeMap<String, ConfigProgress>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigProgress {
    /// `simulation_config.runs`.
    pub runs: u32,
    /// Completed run index -> its CSV row (see `SimulationResults::csv_record`).
    pub rows: BTreeMap<u32, Vec<String>>,
}

impl ConfigProgress {
    pub fn is_complete(&self) -> bool {
        self.rows.len() == self.runs as usize
    }
}

fn unix_now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Checkpoint {
    /// A checkpoint with nothing completed yet, one entry per config.
    pub fn new(configs: &[Config]) -> Checkpoint {
        Checkpoint {
            format: FORMAT.to_string(),
            version: VERSION,
            fingerprint: fingerprint(configs),
            updated_unix_s: unix_now_s(),
            configs: configs
                .iter()
                .map(|c| {
                    (
                        c.simulation_config.csv_output_path.clone(),
                        ConfigProgress {
                            runs: c.simulation_config.runs,
                            rows: BTreeMap::new(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Records a completed run. Panics if `csv_output_path` is not one of the
    /// configs the checkpoint was created for.
    pub fn record(&mut self, csv_output_path: &str, run: u32, row: Vec<String>) {
        self.configs
            .get_mut(csv_output_path)
            .unwrap_or_else(|| panic!("{csv_output_path} is not part of the checkpoint"))
            .rows
            .insert(run, row);
        self.updated_unix_s = unix_now_s();
    }

    pub fn completed_runs(&self, csv_output_path: &str) -> BTreeSet<u32> {
        self.configs
            .get(csv_output_path)
            .map(|c| c.rows.keys().copied().collect())
            .unwrap_or_default()
    }

    /// `(completed runs, total runs)` over all configs.
    pub fn progress(&self) -> (usize, usize) {
        self.configs.values().fold((0, 0), |(done, total), c| {
            (done + c.rows.len(), total + c.runs as usize)
        })
    }
}

/// Stable hash of the parsed configs: every field that affects the results,
/// but not the file's whitespace, key order or number spelling. Identical
/// across processes and Rust versions (FNV-1a over the canonical JSON).
pub fn fingerprint(configs: &[Config]) -> String {
    let canonical = serde_json::to_vec(configs).expect("configs serialize");
    let hash = canonical
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

/// Writes the checkpoint atomically.
pub fn save(path: &Path, checkpoint: &Checkpoint) -> io::Result<()> {
    save_with(path, checkpoint, |_| Ok(()))
}

/// Like [`save`], calling `before_rename` with the completely written and
/// synced temp file first. If it fails, its error is returned and the
/// checkpoint is not replaced (a test seam for a crash before the rename).
pub fn save_with(
    path: &Path,
    checkpoint: &Checkpoint,
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let json = serde_json::to_vec(checkpoint).map_err(io::Error::other)?;
    write_atomic_with(path, &json, before_rename)
}

/// Writes `bytes` to `path` via a temp file in the same directory, fsync and
/// rename.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, bytes, |_| Ok(()))
}

fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut tmp_name: OsString = path
        .file_name()
        .ok_or_else(|| io::Error::other("path has no file name"))?
        .to_owned();
    tmp_name.push(".tmp");
    let tmp: PathBuf = path.with_file_name(tmp_name);

    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    before_rename(&tmp)?;
    fs::rename(&tmp, path)?;
    // Make the rename itself durable.
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    File::open(dir)?.sync_all()
}

/// Strict loader. The error names `path`.
pub fn load(path: &Path) -> Result<Checkpoint, String> {
    let fail = |what: String| format!("{}: {what}", path.display());
    let bytes = fs::read(path).map_err(|e| fail(format!("cannot read: {e}")))?;
    let checkpoint: Checkpoint =
        serde_json::from_slice(&bytes).map_err(|e| fail(format!("cannot parse: {e}")))?;
    if checkpoint.format != FORMAT {
        return Err(fail(format!(
            "format is {:?}, expected {FORMAT:?}",
            checkpoint.format
        )));
    }
    if checkpoint.version != VERSION {
        return Err(fail(format!(
            "version is {}, expected {VERSION}",
            checkpoint.version
        )));
    }
    for (csv, progress) in &checkpoint.configs {
        for (run, row) in &progress.rows {
            if *run >= progress.runs {
                return Err(fail(format!(
                    "{csv}: run index {run} is out of range (config has {} runs)",
                    progress.runs
                )));
            }
            if row.first().map(String::as_str) != Some(run.to_string().as_str()) {
                return Err(fail(format!(
                    "{csv}: the row filed under run {run} says run {:?}",
                    row.first()
                )));
            }
            if row.len() != CSV_HEADER.len() {
                return Err(fail(format!(
                    "{csv}: run {run} has {} fields, expected {}",
                    row.len(),
                    CSV_HEADER.len()
                )));
            }
        }
    }
    Ok(checkpoint)
}
