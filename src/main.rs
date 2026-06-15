use dpds_core::{
    delayer::{Delayer, RVDistribution},
    limiter::{Limiter, LimiterMode},
};
use lib::{
    config::{Config, SimulationConfig},
    context::SimulationContext,
    events::{Event, EventKind, PacketArrivalEvent, PacketTransmissionEvent},
    results::SimulationResults,
    statistics::ReorderingMetric,
};

use csv::Writer;
use indicatif::{ProgressBar, ProgressStyle};
use std::env;
use std::fs::{self, File};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

const RESULT_DIR: &str = "results";
const DEFAULT_MAX_CONCURRENT_SIMULATIONS: usize = 2;

#[tokio::main]
async fn main() {
    // Read simulation configurations from JSON file
    let config_file = fs::read_to_string("simulation_configs.json")
        .expect("Failed to read simulation_configs.json");

    let configs: Vec<Config> =
        serde_json::from_str(&config_file).expect("Failed to parse simulation_configs.json");

    // Read MAX_CONCURRENT_SIMULATIONS from environment variable or use default
    let max_concurrent_simulations = env::var("MAX_CONCURRENT_SIMULATIONS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONCURRENT_SIMULATIONS);

    // Create a semaphore to limit concurrent simulations
    let semaphore = Arc::new(Semaphore::new(max_concurrent_simulations));

    let total_runs: u64 = configs
        .iter()
        .map(|c| c.simulation_config.runs as u64)
        .sum();
    let pb = ProgressBar::new(total_runs);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    for config in configs.into_iter() {
        let runs = config.simulation_config.runs;
        let csv_output_path = config.simulation_config.csv_output_path.clone();

        // Ensure results directory exists
        fs::create_dir_all(RESULT_DIR).expect("Failed to create results directory");

        // Create CSV writer with mutex for thread-safe writing
        let file = File::create(format!("{RESULT_DIR}/{csv_output_path}"))
            .unwrap_or_else(|_| panic!("Failed to create CSV file: {}", csv_output_path));
        let csv_writer = Arc::new(Mutex::new(Writer::from_writer(file)));

        // Write CSV header
        {
            let mut writer = csv_writer.lock().unwrap();
            writer
                .write_record([
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
                    "dropped_packets_bandwidth",
                    "burst_spacing_offset_mean_ns",
                    "burst_spacing_offset_std_ns",
                    "burst_spacing_offset_max_ns",
                    "dropped_packets_burst_virtual_queue",
                ])
                .expect("Failed to write CSV header");
            writer.flush().expect("Failed to flush CSV header");
        }

        // Collect all simulation tasks for one config
        let mut tasks = Vec::new();

        for run in 0..runs {
            let config_clone = config.clone();
            let csv_output_path_clone = csv_output_path.clone();
            let csv_writer_clone = Arc::clone(&csv_writer);
            let semaphore_clone = Arc::clone(&semaphore);
            let pb_clone = pb.clone();
            let task = tokio::spawn(async move {
                // Acquire semaphore permit before starting simulation
                let _permit = semaphore_clone.acquire().await.unwrap();
                pb_clone.set_message(format!("{csv_output_path_clone} run {}/{}", run + 1, runs));
                let results = run_simulation(config_clone);
                results
                    .write_to_csv(&csv_writer_clone, run)
                    .expect("Failed to write results to CSV");
                pb_clone.inc(1);
                // Permit is automatically released when _permit is dropped
            });
            tasks.push(task);
        }

        for task in tasks {
            task.await.expect("Simulation task panicked");
        }
    }

    pb.finish_with_message("All simulations completed");
}

fn run_simulation(config: Config) -> SimulationResults {
    // Initialize simulation context with all running variables
    let mut simulation_context = initialize_simulation_context(&config);

    let simulation_config = &config.simulation_config;

    let rate_pps = simulation_config.rate_gbps * 1e9
        / ((simulation_config.average_frame_size_byte + 20.) * 8.);
    let rx_inter_arrival_time_ns = 1e9 / rate_pps;
    let simulation_duration_ns = (simulation_config.duration_s * 1e9).round() as u64;

    // Do the simulation
    while !simulation_context.events_is_empty() {
        let event = simulation_context.events_pop();

        // Do not go back in time, even when the packet was scheduled for the past and needs to be send immediately
        simulation_context.current_simulation_time_ns = f64::max(
            simulation_context.current_simulation_time_ns,
            event.time_ns(),
        );

        match event.kind() {
            // Only add new packet arrivals until simulation end is reached
            EventKind::PacketArrival(current_arrival_event)
                if simulation_context.current_simulation_time_ns
                    <= simulation_duration_ns as f64 =>
            {
                let next_arrival_event =
                    PacketArrivalEvent::new(current_arrival_event.sequence_number() + 1);

                // Push new packet arrival
                simulation_context.events_push(Event::new(
                    simulation_context.current_simulation_time_ns + rx_inter_arrival_time_ns,
                    EventKind::PacketArrival(next_arrival_event),
                ));

                // Process this packet for transmission
                let delay_ns = simulation_context
                    .delayer
                    .delayer_sample_ns(simulation_context.current_simulation_time_ns);

                // Record theoretical passage time directly after sampling the delay
                simulation_context.statistics.record_theoretical_delay(
                    delay_ns,
                    simulation_context.current_simulation_time_ns,
                );

                let time_to_send_ns = simulation_context.current_simulation_time_ns + delay_ns;

                let transmission_event = if simulation_config.activate_reordering {
                    EventKind::PacketTransmissionOrderTime(PacketTransmissionEvent::new(
                        simulation_context.current_simulation_time_ns,
                        current_arrival_event.sequence_number(),
                    ))
                } else {
                    EventKind::PacketTransmissionOrderSequence(PacketTransmissionEvent::new(
                        simulation_context.current_simulation_time_ns,
                        current_arrival_event.sequence_number(),
                    ))
                };

                simulation_context.events_push(Event::new(time_to_send_ns, transmission_event));
            }
            EventKind::PacketArrival(_) => continue,
            EventKind::PacketTransmissionOrderSequence(packet_transmission_event) => {
                handle_packet_transmission(
                    simulation_config,
                    &mut simulation_context,
                    packet_transmission_event,
                )
            }
            EventKind::PacketTransmissionOrderTime(packet_transmission_event) => {
                handle_packet_transmission(
                    simulation_config,
                    &mut simulation_context,
                    packet_transmission_event,
                )
            }
        }
    }

    simulation_context.statistics.get_results()
}

fn initialize_simulation_context(config: &Config) -> SimulationContext {
    let simulation_config = &config.simulation_config;
    // Use the configured reordering metric or fall back to the default (OutOfOrderCounter)
    let reordering_metric = simulation_config
        .reordering_metric
        .unwrap_or(ReorderingMetric::OutOfOrderCounter);
    let rate_pps = simulation_config.rate_gbps * 1e9
        / ((simulation_config.average_frame_size_byte + 20.) * 8.);
    // Process delay config
    let delay_config = &config.delay_config;
    let delayer = match delay_config {
        Some(delay_config) => Delayer::new(
            delay_config.rv_distribution,
            delay_config.mean_ns,
            delay_config.std_ns,
            delay_config.activate_correlation,
            delay_config.half_life_period_ns,
            rate_pps,
        ),
        None => Delayer::new(
            RVDistribution::DeterministicDistribution,
            0.,
            0.,
            false,
            0.,
            0.,
        ),
    };

    // Process limiter config
    let limiter_config = &config.limiter_config;
    let limiter = match limiter_config {
        Some(limiter_config) => Limiter::new(
            limiter_config.mode,
            true,
            limiter_config.rate_bps,
            limiter_config.limit_bit,
        ),
        None => Limiter::new(LimiterMode::Spacer, false, 0., 0.),
    };

    // Use X% utilization for virtual queue -> div by X%
    let virtual_queue_rate_bps = simulation_config.rate_gbps * 1e9 / 0.90;
    let burst_virtual_queue =
        Limiter::new(LimiterMode::Spacer, true, virtual_queue_rate_bps, f64::MAX);

    let mut context =
        SimulationContext::new(delayer, limiter, burst_virtual_queue, reordering_metric);

    // Initialize passage time tracker if delay config is present
    if let Some(delay_config) = delay_config {
        context
            .statistics
            .init_passage_time_tracker(delay_config.mean_ns, delay_config.std_ns);
    }

    context
}

fn handle_packet_transmission(
    simulation_config: &SimulationConfig,
    simulation_context: &mut SimulationContext,
    packet_transmission_event: &PacketTransmissionEvent,
) {
    // Do not measure statistics of first packet to send
    if simulation_context.last_transmission_time_ns == 0. {
        simulation_context.last_transmission_time_ns =
            simulation_context.current_simulation_time_ns;
        return;
    }

    let limiter_result = simulation_context.limiter.limiter_calculate_offset_ns(
        simulation_context.current_simulation_time_ns,
        simulation_config.average_frame_size_byte.ceil() as u32,
    );

    if let Some(transmission_offset_ns) = limiter_result {
        // Update time to sent with the limiter offset
        simulation_context.current_simulation_time_ns += transmission_offset_ns;
    } else {
        simulation_context
            .statistics
            .record_dropped_packet_bandwidth();
    }

    // Max with 0 to compensate if a packet should have been sent in the past and therefore needs to be sent immediately
    let inter_arrival_time_ns = (simulation_context.current_simulation_time_ns
        - simulation_context.last_transmission_time_ns)
        .max(0.);

    simulation_context
        .statistics
        .record_inter_arrival_time(inter_arrival_time_ns);

    simulation_context.last_transmission_time_ns = simulation_context.current_simulation_time_ns;

    let delay_ns =
        simulation_context.current_simulation_time_ns - packet_transmission_event.rx_timestamp_ns();
    simulation_context
        .statistics
        .record_delay(delay_ns, simulation_context.current_simulation_time_ns);

    // Record sequence number for Kendall Tau Distance calculation
    simulation_context
        .statistics
        .record_sequence_number(packet_transmission_event.sequence_number());

    let burst_offset = simulation_context
        .burst_virtual_queue
        .limiter_calculate_offset_ns(
            simulation_context.current_simulation_time_ns,
            simulation_config.average_frame_size_byte.ceil() as u32,
        );
    if let Some(offset_ns) = burst_offset {
        simulation_context
            .statistics
            .record_burst_spacing_offset(offset_ns);
    } else {
        simulation_context
            .statistics
            .record_dropped_packet_burst_virtual_queue();
    }
}
