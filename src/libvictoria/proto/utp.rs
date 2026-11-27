use std::time::{Duration, Instant};

enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

enum HeaderExtensionType {
    Termination = 0,
    SelectiveAcknowledge = 1,
}

enum HeaderExtension {
    SelectiveAcknowledge {
        bitmask: Vec<u32>,
    },
}

struct PacketHeader {
    of_type: PacketType,
    version: u8,
    connection_id: u16,
    timestamp: Instant,
    timestamp_difference: Duration,
    window_size: u32,
    sequence_number: u16,
    acknowledge_number: u16,
    extensions: Vec<HeaderExtension>
}
