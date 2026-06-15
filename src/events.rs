#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacketArrivalEvent {
    // Implicitely track sequence numbers by arrival events
    sequence_number: u64,
}

impl PacketArrivalEvent {
    pub fn new(sequence_number: u64) -> Self {
        Self { sequence_number }
    }

    pub fn sequence_number(&self) -> u64 {
        self.sequence_number
    }
}

#[derive(Debug, Clone)]
pub struct PacketTransmissionEvent {
    rx_timestamp_ns: f64,
    sequence_number: u64,
}

impl PacketTransmissionEvent {
    pub fn new(rx_timestamp_ns: f64, sequence_number: u64) -> Self {
        Self {
            rx_timestamp_ns,
            sequence_number,
        }
    }

    pub fn sequence_number(&self) -> u64 {
        self.sequence_number
    }

    pub fn rx_timestamp_ns(&self) -> f64 {
        self.rx_timestamp_ns
    }
}

#[derive(Debug, Clone)]
pub enum EventKind {
    PacketArrival(PacketArrivalEvent),
    // Orders transmission events according to their time
    PacketTransmissionOrderTime(PacketTransmissionEvent),
    // Orders transmission events according to their sequence number
    PacketTransmissionOrderSequence(PacketTransmissionEvent),
}

#[derive(Debug, Clone)]
pub struct Event {
    time_ns: f64,
    kind: EventKind,
}

impl Event {
    pub fn new(time_ns: f64, kind: EventKind) -> Self {
        Event { time_ns, kind }
    }

    pub fn time_ns(&self) -> f64 {
        self.time_ns
    }

    pub fn kind(&self) -> &EventKind {
        &self.kind
    }
}

impl PartialEq for Event {
    fn eq(&self, other: &Self) -> bool {
        self.time_ns.total_cmp(&other.time_ns).is_eq()
    }
}

impl Eq for Event {}

// Implement Ord and PartialOrd to make Event work with BinaryHeap
// BinaryHeap is a max-heap, but we want events with earlier times first (min-heap)
// So we reverse the comparison order
impl Ord for Event {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Order events according to sequence number if both are sequence number ordered
        if let (
            EventKind::PacketTransmissionOrderSequence(self_kind),
            EventKind::PacketTransmissionOrderSequence(other_kind),
        ) = (&self.kind, &other.kind)
        {
            other_kind.sequence_number.cmp(&self_kind.sequence_number) // Reversed for min-heap behavior
        } else {
            other.time_ns.total_cmp(&self.time_ns) // Reversed for min-heap behavior
        }
    }
}

impl PartialOrd for Event {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
