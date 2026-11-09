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

#[derive(Debug, Clone)]
pub struct SimulationResults {
    pub delay_mean_ns: f64,
    pub delay_std_ns: f64,
    pub kendall_tau_distance_div_n: f64,
    pub out_of_order_packets: usize,
    pub theoretical_passage_time_mean_ns: f64,
    pub dropped_packets_limiter: u64,
    pub dropped_packets_buffer_full: u64,
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
