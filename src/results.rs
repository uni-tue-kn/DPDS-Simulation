use csv::Writer;
use std::fs::File;
use std::sync::{Arc, Mutex};

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
    pub dropped_packets_bandwidth: u64,
    pub burst_spacing_offset_mean_ns: f64,
    pub burst_spacing_offset_std_ns: f64,
    pub burst_spacing_offset_max_ns: u64,
    pub dropped_packets_burst_virtual_queue: u64,
}

impl SimulationResults {
    pub fn write_to_csv(
        &self,
        csv_writer: &Arc<Mutex<Writer<File>>>,
        run: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut writer = csv_writer.lock().unwrap();
        writer.write_record(&[
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
            self.dropped_packets_bandwidth.to_string(),
            self.burst_spacing_offset_mean_ns.to_string(),
            self.burst_spacing_offset_std_ns.to_string(),
            self.burst_spacing_offset_max_ns.to_string(),
            self.dropped_packets_burst_virtual_queue.to_string(),
        ])?;
        writer.flush()?;
        Ok(())
    }
}
