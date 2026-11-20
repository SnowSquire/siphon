//! Scalar telemetry. Nothing here holds a `Segment`, a `Reader` or a spool:
//! cloning those would pin arena slots, so only counters and scalars survive.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use siphon_shared::rpc::SegmentRecord;
use siphon_shared::rpc::StreamKey;
use siphon_shared::rpc::Update;

use crate::bus::Bus;

/// Segments kept per pipeline for `ListSegments` and the UI ring.
const RING: usize = 64;

/// One running pipeline's counters and segment ring.
pub struct StreamMetrics {
    pub key: StreamKey,
    pub polls: Cell<u64>,
    pub poll_failures: Cell<u64>,
    pub segment_tail: Cell<u64>,
    pub prefetch_tail: Cell<u64>,
    pub window: Cell<u32>,
    pub segments: RefCell<VecDeque<SegmentRecord>>,
    pub last_error: RefCell<Option<String>>,
    variant: RefCell<Option<String>>,
    bus: Rc<Bus>,
}

impl StreamMetrics {
    #[must_use]
    pub fn new(key: StreamKey, variant: Option<String>, bus: Rc<Bus>) -> Rc<Self> {
        Rc::new(Self {
            key,
            polls: Cell::new(0),
            poll_failures: Cell::new(0),
            segment_tail: Cell::new(0),
            prefetch_tail: Cell::new(0),
            window: Cell::new(0),
            segments: RefCell::new(VecDeque::with_capacity(RING)),
            last_error: RefCell::new(None),
            variant: RefCell::new(variant),
            bus,
        })
    }

    #[must_use]
    pub fn variant(&self) -> Option<String> {
        self.variant.borrow().clone()
    }

    pub fn set_variant(&self, variant: Option<String>) {
        *self.variant.borrow_mut() = variant;
    }

    pub fn set_error(&self, error: Option<String>) {
        *self.last_error.borrow_mut() = error;
    }

    /// Appends one completed fetch and publishes it to the UI.
    pub fn push_segment(&self, record: SegmentRecord) {
        {
            let mut ring = self.segments.borrow_mut();
            if ring.len() == RING {
                ring.pop_front();
            }
            ring.push_back(record.clone());
        }
        self.bus.publish(Update::Segment {
            key: self.key.clone(),
            record,
        });
    }

    /// The newest `limit` records, oldest first.
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<SegmentRecord> {
        let ring = self.segments.borrow();
        let skip = ring.len().saturating_sub(limit);
        ring.iter().skip(skip).cloned().collect()
    }
}

/// What the downloader workers share for the snapshot.
pub struct DownloaderMetrics {
    pub capacity: u32,
    pub queued: Cell<u32>,
    pub in_flight: Cell<u32>,
    pub retries: Cell<u64>,
}

impl DownloaderMetrics {
    #[must_use]
    pub const fn new(capacity: u32) -> Self {
        Self {
            capacity,
            queued: Cell::new(0),
            in_flight: Cell::new(0),
            retries: Cell::new(0),
        }
    }
}

/// Live counters from one recording task. The disk owns the values; the
/// registry only reads them for `GetDisk`.
pub struct DiskMetrics {
    pub active: Cell<bool>,
    pub runs: Cell<u32>,
    pub segments: Cell<u64>,
    pub bytes: Cell<u64>,
    pub position_ms: Cell<u64>,
    pub last_error: RefCell<Option<String>>,
}

impl DiskMetrics {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            active: Cell::new(false),
            runs: Cell::new(0),
            segments: Cell::new(0),
            bytes: Cell::new(0),
            position_ms: Cell::new(0),
            last_error: RefCell::new(None),
        }
    }
}

impl Default for DiskMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskMetrics {
    pub fn note_written(&self, position_ms: u64, bytes: u64) {
        self.segments.set(self.segments.get().saturating_add(1));
        self.bytes.set(self.bytes.get().saturating_add(bytes));
        self.position_ms
            .set(self.position_ms.get().max(position_ms));
    }
}

/// Identifies the segment a fetch belongs to, for the ring entry it produces.
#[derive(Clone)]
pub struct FetchStats {
    pub metrics: Rc<StreamMetrics>,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub duration_ms: u64,
    pub pdt_ms: u64,
}
