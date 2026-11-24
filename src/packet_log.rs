// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Bounded in-memory log of MQTT packet traffic.

use std::collections::VecDeque;

use jiff::ZonedDateTime;

pub const DEFAULT_CAPACITY: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Sent,
    Received,
}

impl Direction {
    pub fn arrow(self) -> &'static str {
        match self {
            Direction::Sent => "→",
            Direction::Received => "←",
        }
    }
}

/// A PUBLISH (sent or received) or a sent SUBSCRIBE, kept in full so it can be repeated from
/// the command line or exported (`details` is truncated for display).
#[derive(Debug, Clone, PartialEq)]
pub enum Replay {
    Publish {
        topic: String,
        qos: u8,
        retain: bool,
        /// UTF-8 text; binary payloads aren't kept.
        payload: String,
        /// MQTT 5 User Properties (empty for 3.1.1).
        user_properties: Vec<(String, String)>,
    },
    Subscribe {
        filter: String,
        qos: u8,
        user_properties: Vec<(String, String)>,
    },
}

/// Payloads larger than this aren't kept for [`Replay`].
pub const REPLAY_PAYLOAD_MAX: usize = 64 * 1024;

impl Replay {
    /// A replayable publish, if the payload is text and not too large.
    pub fn publish(
        topic: &str,
        qos: u8,
        retain: bool,
        payload: &[u8],
        user_properties: &[(String, String)],
    ) -> Option<Self> {
        if payload.len() > REPLAY_PAYLOAD_MAX {
            return None;
        }
        let payload = std::str::from_utf8(payload).ok()?.to_owned();
        Some(Replay::Publish {
            topic: topic.to_owned(),
            qos,
            retain,
            payload,
            user_properties: user_properties.to_vec(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct PacketLogEntry {
    pub timestamp: ZonedDateTime,
    pub direction: Direction,
    /// Packet type, e.g. `CONNECT`, `PUBLISH`.
    pub kind: String,
    pub details: String,
    /// Set for PUBLISH packets (both directions) and sent SUBSCRIBE packets.
    pub replay: Option<Replay>,
}

impl PacketLogEntry {
    pub fn new(direction: Direction, kind: impl Into<String>, details: impl Into<String>) -> Self {
        Self {
            timestamp: ZonedDateTime::now(),
            direction,
            kind: kind.into(),
            details: details.into(),
            replay: None,
        }
    }

    pub fn with_replay(mut self, replay: Option<Replay>) -> Self {
        self.replay = replay;
        self
    }

    pub fn is_ping(&self) -> bool {
        self.kind == "PINGREQ" || self.kind == "PINGRESP"
    }
}

/// Ring buffer: once `capacity` is reached, the oldest entry is dropped.
#[derive(Debug)]
pub struct PacketLog {
    entries: VecDeque<PacketLogEntry>,
    capacity: usize,
}

impl Default for PacketLog {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl PacketLog {
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, entry: PacketLogEntry) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &PacketLogEntry> + ExactSizeIterator {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Renders a payload for display: UTF-8 text if possible, otherwise hex. Truncated to `max` bytes.
pub fn format_payload(payload: &[u8], max: usize) -> String {
    let (shown, truncated) = if payload.len() > max {
        (&payload[..max], true)
    } else {
        (payload, false)
    };
    let mut out = match std::str::from_utf8(shown) {
        Ok(s) => s.to_owned(),
        Err(_) => {
            let hex: String = shown.iter().map(|b| format!("{b:02x}")).collect();
            format!("0x{hex}")
        }
    };
    if truncated {
        out.push_str(&format!(" … ({} bytes total)", payload.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_payloads_within_the_limit_are_replayable() {
        assert!(Replay::publish("t", 0, false, b"{\"a\":1}", &[]).is_some());
        assert!(
            Replay::publish("t", 0, false, &[0xff, 0xfe], &[]).is_none(),
            "not UTF-8"
        );
        let big = vec![b'x'; REPLAY_PAYLOAD_MAX + 1];
        assert!(Replay::publish("t", 0, false, &big, &[]).is_none());
    }
    use proptest::prelude::*;

    #[test]
    fn drops_oldest_when_full() {
        let mut log = PacketLog::with_capacity(2);
        log.push(PacketLogEntry::new(Direction::Sent, "A", ""));
        log.push(PacketLogEntry::new(Direction::Sent, "B", ""));
        log.push(PacketLogEntry::new(Direction::Received, "C", ""));
        let kinds: Vec<_> = log.entries().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, ["B", "C"]);
        log.clear();
        assert!(log.is_empty());
    }

    /// The log and message views format timestamps as `HH:MM:SS.mmm`.
    #[test]
    fn timestamps_format_with_milliseconds() {
        let ts = jiff::Timestamp::from_millisecond(1_700_000_000_123).unwrap();
        let zoned = ts.to_zoned_datetime(jiff::tz::TimeZone::UTC);
        assert_eq!(zoned.strftime("%H:%M:%S%.3f").to_string(), "22:13:20.123");
        let entry = PacketLogEntry::new(Direction::Sent, "PING", "");
        let shown = entry.timestamp.strftime("%H:%M:%S%.3f").to_string();
        assert_eq!(shown.len(), 12, "{shown}");
    }

    #[test]
    fn payload_formatting() {
        assert_eq!(format_payload(b"hello", 100), "hello");
        assert_eq!(format_payload(&[0xff, 0x00], 100), "0xff00");
        assert!(format_payload(b"hello world", 5).starts_with("hello …"));
    }

    proptest! {
        #[test]
        fn never_exceeds_capacity(cap in 1usize..50, n in 0usize..200) {
            let mut log = PacketLog::with_capacity(cap);
            for i in 0..n {
                log.push(PacketLogEntry::new(Direction::Sent, i.to_string(), ""));
            }
            prop_assert_eq!(log.len(), n.min(cap));
            // Newest entry is always last.
            if n > 0 {
                let last = log.entries().last().unwrap().kind.clone();
                prop_assert_eq!(last, (n - 1).to_string());
            }
        }
    }
}
