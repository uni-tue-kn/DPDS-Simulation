use std::collections::BinaryHeap;

use dpds_core::{delayer::Delayer, limiter::Limiter};

use crate::events::{Event, EventKind, PacketArrivalEvent};
use crate::statistics::{ReorderingMetric, SimulationStatistics};

pub struct SimulationContext {
    pub current_simulation_time_ns: f64,
    pub last_transmission_time_ns: f64,
    // Two event queues needed since ordering RX and TX events has side effects
    rx_events: BinaryHeap<Event>,
    // Always reorder events but transmission events are kept in sequence with special transmission event type
    tx_events: BinaryHeap<Event>,
    pub delayer: Delayer,
    pub limiter: Limiter,
    pub burst_virtual_queue: Limiter,
    pub statistics: SimulationStatistics,
}

impl SimulationContext {
    pub fn new(
        delayer: Delayer,
        limiter: Limiter,
        burst_virtual_queue: Limiter,
        reordering_metric: ReorderingMetric,
    ) -> Self {
        let mut rx_events = BinaryHeap::new();
        let tx_events = BinaryHeap::new();

        rx_events.push(Event::new(
            0.,
            EventKind::PacketArrival(PacketArrivalEvent::new(0)),
        ));

        Self {
            current_simulation_time_ns: 0.,
            last_transmission_time_ns: 0.,
            rx_events,
            tx_events,
            delayer,
            limiter,
            burst_virtual_queue,
            statistics: SimulationStatistics::new(reordering_metric),
        }
    }

    pub fn events_is_empty(&self) -> bool {
        self.rx_events.is_empty() && self.tx_events.is_empty()
    }

    pub fn events_pop(&mut self) -> Event {
        // Calculate which event queue to use next
        let next_rx_event = self.rx_events.peek();
        let next_rx_event_time_ns = match next_rx_event {
            Some(rx_event) => rx_event.time_ns(),
            None => f64::INFINITY,
        };

        let next_tx_event = self.tx_events.peek();
        let next_tx_event_time_ns = match next_tx_event {
            Some(tx_event) => tx_event.time_ns(),
            None => f64::INFINITY,
        };

        let event_queue = if next_rx_event_time_ns <= next_tx_event_time_ns {
            &mut self.rx_events
        } else {
            &mut self.tx_events
        };

        // Process next event
        event_queue
            .pop()
            .unwrap_or_else(|| panic!("Events should not be empty!"))
    }

    pub fn events_push(&mut self, event: Event) {
        match event.kind() {
            EventKind::PacketArrival(_) => self.rx_events.push(event),
            EventKind::PacketTransmissionOrderSequence(_) => self.tx_events.push(event),
            EventKind::PacketTransmissionOrderTime(_) => self.tx_events.push(event),
        }
    }
}
