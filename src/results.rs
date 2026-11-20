use csv::Writer;
use std::fs::File;
use std::sync::{Arc, Mutex};

/// Column names of the per-config result CSVs, in the order of
/// [`SimulationResults::csv_record`].
pub const CSV_HEADER: [&str; 8] = [
    "run",
    "delay_mean_ns",
    "delay_std_ns",
    "kendall_tau_distance_div_n",
    "out_of_order_packets",
    "theoretical_passage_time_mean_ns",
    "dropped_packets_limiter",
    "dropped_packets_buffer_full",
];

/// Column names of the per-config quantile CSVs.
pub const QUANTILE_CSV_HEADER: [&str; 3] = ["run", "probability", "delay_ns"];

/// Probabilities at which the achieved delay is reported: 0.001, 0.005,
/// 0.01 to 0.99 in steps of 0.01, 0.995 and 0.999.
pub const QUANTILE_PROBABILITIES: [f64; 103] = {
    let mut p = [0.0; 103];
    p[0] = 0.001;
    p[1] = 0.005;
    let mut k = 1;
    while k <= 99 {
        // A single correctly rounded division prints as `0.07`, not `0.07000000000000001`.
        p[k + 1] = k as f64 / 100.0;
        k += 1;
    }
    p[101] = 0.995;
    p[102] = 0.999;
    p
};

#[derive(Debug, Clone)]
pub struct SimulationResults {
    pub delay_mean_ns: f64,
    pub delay_std_ns: f64,
    pub kendall_tau_distance_div_n: f64,
    pub out_of_order_packets: usize,
    pub theoretical_passage_time_mean_ns: f64,
    pub dropped_packets_limiter: u64,
    pub dropped_packets_buffer_full: u64,
    /// The achieved delay at each of [`QUANTILE_PROBABILITIES`], in ns (the
    /// delay histogram has 3 significant figures).
    pub delay_quantiles_ns: Vec<u64>,
}

impl SimulationResults {
    /// One CSV row (see [`CSV_HEADER`]) for the given run index.
    pub fn csv_record(&self, run: u32) -> [String; 8] {
        [
            run.to_string(),
            self.delay_mean_ns.to_string(),
            self.delay_std_ns.to_string(),
            self.kendall_tau_distance_div_n.to_string(),
            self.out_of_order_packets.to_string(),
            self.theoretical_passage_time_mean_ns.to_string(),
            self.dropped_packets_limiter.to_string(),
            self.dropped_packets_buffer_full.to_string(),
        ]
    }

    pub fn write_to_csv(
        &self,
        csv_writer: &Arc<Mutex<Writer<File>>>,
        run: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut writer = csv_writer.lock().unwrap();
        writer.write_record(self.csv_record(run))?;
        writer.flush()?;
        Ok(())
    }
}

/// The text of a complete result CSV: the header and the given rows (in the
/// order given).
pub fn csv_bytes<'a>(
    rows: impl IntoIterator<Item = &'a Vec<String>>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut writer = Writer::from_writer(Vec::new());
    writer.write_record(CSV_HEADER)?;
    for row in rows {
        writer.write_record(row)?;
    }
    Ok(writer.into_inner()?)
}

/// The text of a complete quantile CSV: one row per (run, probability), in the
/// order of the given runs and of [`QUANTILE_PROBABILITIES`].
pub fn quantiles_csv_bytes<'a>(
    runs: impl IntoIterator<Item = (&'a u32, &'a Vec<u64>)>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut writer = Writer::from_writer(Vec::new());
    writer.write_record(QUANTILE_CSV_HEADER)?;
    for (run, quantiles) in runs {
        for (probability, delay_ns) in QUANTILE_PROBABILITIES.iter().zip(quantiles) {
            writer.write_record([
                run.to_string(),
                probability.to_string(),
                delay_ns.to_string(),
            ])?;
        }
    }
    Ok(writer.into_inner()?)
}
