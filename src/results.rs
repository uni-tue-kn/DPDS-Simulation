use csv::Writer;
use std::fs::File;
use std::sync::{Arc, Mutex};

/// Column names of the per-config result CSVs, in the order of
/// [`SimulationResults::csv_record`].
pub const CSV_HEADER: [&str; 23] = [
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
    "dropped_packets_loss",
    "dropped_packets_limiter",
    "dropped_packets_buffer_full",
    "burst_spacing_offset_mean_ns",
    "burst_spacing_offset_std_ns",
    "burst_spacing_offset_max_ns",
    "dropped_packets_burst_virtual_queue",
];

#[derive(Debug, Clone)]
pub struct SimulationResults {
    pub tracked_packets: usize,
    pub inter_arrival_time_mean_ns: f64,
    pub inter_arrival_time_std_ns: f64,
    pub delay_mean_ns: f64,
    pub delay_std_ns: f64,
    pub kendall_tau_distance: usize,
    pub kendall_tau_distance_div_n: f64,
    pub out_of_order_packets: usize,
    pub out_of_order_packets_div_n: f64,
    pub tracked_theoretical_passage_times: usize,
    pub theoretical_passage_time_mean_ns: f64,
    pub theoretical_passage_time_std_ns: f64,
    pub tracked_measured_passage_times: usize,
    pub measured_passage_time_mean_ns: f64,
    pub measured_passage_time_std_ns: f64,
    pub dropped_packets_loss: u64,
    pub dropped_packets_limiter: u64,
    pub dropped_packets_buffer_full: u64,
    pub burst_spacing_offset_mean_ns: f64,
    pub burst_spacing_offset_std_ns: f64,
    pub burst_spacing_offset_max_ns: u64,
    pub dropped_packets_burst_virtual_queue: u64,
}

impl SimulationResults {
    /// One CSV row (see [`CSV_HEADER`]) for the given run index.
    pub fn csv_record(&self, run: u32) -> [String; 23] {
        [
            run.to_string(),
            self.tracked_packets.to_string(),
            self.inter_arrival_time_mean_ns.to_string(),
            self.inter_arrival_time_std_ns.to_string(),
            self.delay_mean_ns.to_string(),
            self.delay_std_ns.to_string(),
            self.kendall_tau_distance.to_string(),
            self.kendall_tau_distance_div_n.to_string(),
            self.out_of_order_packets.to_string(),
            self.out_of_order_packets_div_n.to_string(),
            self.tracked_theoretical_passage_times.to_string(),
            self.theoretical_passage_time_mean_ns.to_string(),
            self.theoretical_passage_time_std_ns.to_string(),
            self.tracked_measured_passage_times.to_string(),
            self.measured_passage_time_mean_ns.to_string(),
            self.measured_passage_time_std_ns.to_string(),
            self.dropped_packets_loss.to_string(),
            self.dropped_packets_limiter.to_string(),
            self.dropped_packets_buffer_full.to_string(),
            self.burst_spacing_offset_mean_ns.to_string(),
            self.burst_spacing_offset_std_ns.to_string(),
            self.burst_spacing_offset_max_ns.to_string(),
            self.dropped_packets_burst_virtual_queue.to_string(),
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
