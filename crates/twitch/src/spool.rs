//! A spool: the byte stream of one HLS segment.
//!
//! The downloader task appends into the spool's current slot; consumers hold a
//! [`Reader`] cursor and get owned, pinned [`SlotView`]s of the published bytes, so
//! TCP and disk writes hand arena memory straight to the kernel.
//!
//! ## State
//!
//! A spool owns one [`Spool`] block — the shared message box between its writer
//! and its readers — plus a one-way chain of arena slots. The block holds only the
//! published total and the closed flag; everything else a reader needs is on the
//! slot it is currently pinning. `SlotRef: Clone` duplicates the cursor, so a
//! consumer can be handed the same spool at the same position; there is no rewind.
//!
//! ## Wakeups
//!
//! There is no wait list on the spool. Every consumer task owns one
//! [`ConsumerWake`] slot (one `Waker`, allocated once when the task starts) and
//! registers it in the scheduler's [`WakeSet`]. A publish wakes every registered
//! consumer; a consumer parked on a different spool re-checks its own frontier and
//! parks again, which costs one poll and keeps the per-spool state down to the
//! block itself.

use std::cell::Cell;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::rc::Weak;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

pub use crate::arena::ArenaFull;
pub use crate::arena::ArenaSet;
use crate::arena::SlotMut;
pub use crate::arena::SlotRef;
pub use crate::arena::SlotView;
use crate::constants::SEGMENT_SLOTS_MAX;

pub struct ConsumerWake {
    waker: Cell<Option<Waker>>,
}

impl ConsumerWake {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            waker: Cell::new(None),
        })
    }

    pub(crate) fn park(&self, context: &Context<'_>) {
        self.waker.set(Some(context.waker().clone()));
    }

    pub(crate) fn wake(&self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}
/// The consumers a publish must wake.
///
/// One list per scheduler, allocated once; entries are weak so a consumer that
/// disconnects is pruned on the next publish instead of leaking.
#[derive(Default)]
pub struct WakeSet {
    consumers: RefCell<Vec<Weak<ConsumerWake>>>,
}

impl WakeSet {
    #[must_use]
    pub fn new() -> Rc<Self> {
        Rc::new(Self::default())
    }

    pub fn register(&self, wake: &Rc<ConsumerWake>) {
        self.consumers.borrow_mut().push(Rc::downgrade(wake));
    }

    fn wake_all(&self) {
        // Holding the borrow across `wake` is fine: compio's waker schedules the
        // task, and a consumer only registers while parking, never during a wake.
        self.consumers.borrow_mut().retain(|weak| {
            weak.upgrade().is_some_and(|wake| {
                wake.wake();
                true
            })
        });
    }
}

/// A spool: one writer, any number of readers, one segment's worth of bytes.
///
/// The writer block and the refcount: the writer plus every reader hold one
/// `Rc`, and the last of them frees it.
pub struct Spool {
    arena: Rc<ArenaSet>,
    /// Total bytes published, the change signal readers park on.
    published: Cell<usize>,
    /// The writer is gone, so the end of the chain is the end of the stream.
    closed: Cell<bool>,
}

impl Spool {
    pub fn open(arena: &Rc<ArenaSet>, wakes: &Rc<WakeSet>) -> Result<(Writer, Reader), ArenaFull> {
        Self::open_with_cap(arena, wakes, SEGMENT_SLOTS_MAX)
    }

    /// A spool with an explicit slot cap. The DVR chunks are much larger than a
    /// live segment, so the disk output opens them against its own arena and cap
    /// instead of contending with (and overflowing) the live pool.
    pub fn open_with_cap(
        arena: &Rc<ArenaSet>,
        wakes: &Rc<WakeSet>,
        cap: usize,
    ) -> Result<(Writer, Reader), ArenaFull> {
        let head = arena.alloc().ok_or(ArenaFull)?;
        let spool = Rc::new(Self {
            arena: Rc::clone(arena),
            published: Cell::new(0),
            closed: Cell::new(false),
        });

        let reader = Reader {
            spool: Rc::clone(&spool),
            cursor: SlotRef::new(arena, head),
        };
        let writer = Writer {
            spool,
            wakes: Rc::clone(wakes),
            tail: SlotMut::adopt(arena, head),
            slots: 1,
            slots_max: cap,
        };

        Ok((writer, reader))
    }
}

pub struct Writer {
    spool: Rc<Spool>,
    wakes: Rc<WakeSet>,
    tail: SlotMut,
    /// Slots this spool owns, capped so one lagging segment cannot pin the pool.
    slots: usize,
    slots_max: usize,
}

impl Writer {
    /// Writable remainder of the tail slot, linking a new slot when it is full.
    pub fn tail(&mut self) -> Result<&mut [u8], ArenaFull> {
        if self.tail.is_full() {
            if self.slots >= self.slots_max {
                return Err(ArenaFull);
            }
            let id = self.spool.arena.alloc().ok_or(ArenaFull)?;
            let next = SlotMut::adopt(&self.spool.arena, id);
            self.tail.id().link(id);
            self.tail = next;
            self.slots += 1;
        }
        Ok(self.tail.tail())
    }

    pub fn commit(&self, len: usize) {
        let published = self.tail.published() + len;
        self.tail.publish(published);
        let total = self.spool.published.get() + len;
        self.spool.published.set(total);
        self.wakes.wake_all();
    }

    /// Nothing has been written yet, so a retry cannot corrupt the spool.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spool.published.get() == 0
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.spool.closed.set(true);
        self.wakes.wake_all();
    }
}

pub struct Reader {
    spool: Rc<Spool>,
    cursor: SlotRef,
}

impl Reader {
    /// Next published run as an owned, pinned view. `None` once the writer is gone
    /// and everything has been consumed.
    ///
    /// `wake` is the caller's own slot; it is parked here while there is nothing to
    /// read.
    pub async fn next_view(&mut self, wake: &ConsumerWake) -> Option<SlotView> {
        loop {
            let seen = self.spool.published.get();

            if let Some(range) = self.cursor.take_range() {
                return Some(SlotView::new(&self.spool.arena, self.cursor.id(), range));
            }

            if self.cursor.id().next().is_some() {
                self.cursor.advance();
                continue;
            }

            if self.spool.closed.get() {
                return None;
            }

            Wait {
                spool: &self.spool,
                wake,
                seen,
            }
            .await;
        }
    }
}

impl Clone for Reader {
    /// A cursor at the same position, for handing the same spool to another
    /// consumer. It continues where the source is; it never rewinds.
    fn clone(&self) -> Self {
        Self {
            spool: Rc::clone(&self.spool),
            cursor: self.cursor.clone(),
        }
    }
}

#[allow(clippy::missing_fields_in_debug)] // the spool is shared state, not a cursor
impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("cursor", &self.cursor.id())
            .field("offset", &self.cursor.offset())
            .finish()
    }
}

struct Wait<'a> {
    spool: &'a Spool,
    wake: &'a ConsumerWake,
    /// Published total observed before checking the cursor.
    seen: usize,
}

impl Future for Wait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let changed = || self.spool.closed.get() || self.spool.published.get() != self.seen;
        if changed() {
            return Poll::Ready(());
        }

        self.wake.park(context);

        // Re-check after parking so a publish between the first check and the
        // registration cannot be lost.
        if changed() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;
    use std::sync::atomic::Ordering;

    use compio::buf::IoBuf;

    use super::*;

    fn run<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);

        for _ in 0..1_000 {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::hint::spin_loop(),
            }
        }
        panic!("future did not progress");
    }

    fn arena() -> Rc<ArenaSet> {
        Rc::new(ArenaSet::new())
    }

    #[test]
    fn streams_published_bytes_to_a_reader() {
        let arena = arena();
        let wakes = WakeSet::new();
        let wake = ConsumerWake::new();
        let (mut writer, reader) = Spool::open(&arena, &wakes).expect("spool");

        let bytes = run(async {
            writer.tail().expect("tail")[..5].copy_from_slice(b"hello");
            writer.commit(5);
            writer.tail().expect("tail")[..6].copy_from_slice(b" world");
            writer.commit(6);

            drop(writer);

            let mut reader = reader;
            let mut out = Vec::new();
            while let Some(view) = reader.next_view(&wake).await {
                out.extend_from_slice(view.as_init());
            }
            out
        });

        assert_eq!(bytes, b"hello world");
    }

    #[test]
    fn publishes_are_visible_before_the_writer_closes() {
        let arena = arena();
        let wakes = WakeSet::new();
        let wake = ConsumerWake::new();
        let (mut writer, mut reader) = Spool::open(&arena, &wakes).expect("spool");

        let bytes = run(async {
            writer.tail().expect("tail")[..3].copy_from_slice(b"abc");
            writer.commit(3);

            // The writer is still alive: the first publish must already be
            // readable, or live streaming degenerates into one burst per segment.
            let mut out = reader
                .next_view(&wake)
                .await
                .expect("first publish")
                .as_init()
                .to_vec();

            writer.tail().expect("tail")[..3].copy_from_slice(b"def");
            writer.commit(3);
            out.extend_from_slice(
                reader
                    .next_view(&wake)
                    .await
                    .expect("second publish")
                    .as_init(),
            );
            out
        });

        assert_eq!(bytes, b"abcdef");
    }

    #[test]
    fn a_clone_starts_at_the_cursor() {
        let arena = arena();
        let wakes = WakeSet::new();
        let wake = ConsumerWake::new();
        let (mut writer, reader) = Spool::open(&arena, &wakes).expect("spool");

        let (first_out, second_out) = run(async {
            writer.tail().expect("tail")[..1].copy_from_slice(b"a");
            writer.commit(1);

            let mut first = reader;
            let mut first_out = first
                .next_view(&wake)
                .await
                .expect("first publish")
                .as_init()
                .to_vec();

            // Cloned after the first byte was consumed: the clone continues from
            // there instead of replaying the segment.
            let mut second = first.clone();

            writer.tail().expect("tail")[..2].copy_from_slice(b"bc");
            writer.commit(2);
            drop(writer);

            while let Some(view) = first.next_view(&wake).await {
                first_out.extend_from_slice(view.as_init());
            }
            let mut second_out = Vec::new();
            while let Some(view) = second.next_view(&wake).await {
                second_out.extend_from_slice(view.as_init());
            }
            (first_out, second_out)
        });

        assert_eq!(first_out, b"abc");
        assert_eq!(second_out, b"bc");
    }

    #[test]
    fn a_reader_sees_eof_when_the_writer_drops() {
        let arena = arena();
        let wakes = WakeSet::new();
        let wake = ConsumerWake::new();
        let (writer, mut reader) = Spool::open(&arena, &wakes).expect("spool");
        drop(writer);

        assert!(run(async { reader.next_view(&wake).await }).is_none());
    }

    #[test]
    fn slots_cross_boundaries() {
        use crate::constants::SLOT_SIZE;

        let arena = arena();
        let wakes = WakeSet::new();
        let wake = ConsumerWake::new();
        let (mut writer, reader) = Spool::open(&arena, &wakes).expect("spool");

        let count = run(async {
            let mut written = 0;
            while written < SLOT_SIZE + 10 {
                let tail = writer.tail().expect("tail");
                let take = tail.len().min(SLOT_SIZE + 10 - written);
                tail[..take].fill(b'x');
                writer.commit(take);
                written += take;
            }
            drop(writer);

            let mut reader = reader;
            let mut count = 0;
            while let Some(view) = reader.next_view(&wake).await {
                count += view.as_init().len();
            }
            count
        });

        assert_eq!(count, SLOT_SIZE + 10);
    }

    #[test]
    fn a_segment_is_capped_at_a_fixed_number_of_slots() {
        use crate::constants::SLOT_SIZE;

        let arena = arena();
        let wakes = WakeSet::new();
        let (mut writer, _reader) = Spool::open(&arena, &wakes).expect("spool");

        let mut written = 0;
        while let Ok(tail) = writer.tail() {
            let len = tail.len();
            writer.commit(len);
            written += len;
            assert!(
                written <= SEGMENT_SLOTS_MAX * SLOT_SIZE,
                "spool grew past its cap"
            );
        }

        assert_eq!(written, SEGMENT_SLOTS_MAX * SLOT_SIZE);
        assert_eq!(arena.live_slots(), SEGMENT_SLOTS_MAX);
    }

    #[test]
    fn a_publish_wakes_a_parked_consumer() {
        // Exercise the wake path directly: park the wake slot, publish, and check
        // that the task was scheduled.
        let wake = ConsumerWake::new();
        let polls = Arc::new(AtomicU32::new(0));
        let waker = Waker::from(Arc::new(CountingWake(Arc::clone(&polls))));
        let context = Context::from_waker(&waker);
        wake.park(&context);
        wake.wake();
        assert_eq!(polls.load(Ordering::Relaxed), 1);
        // Taking clears the slot: a second wake is a no-op.
        wake.wake();
        assert_eq!(polls.load(Ordering::Relaxed), 1);
    }

    struct CountingWake(Arc<AtomicU32>);

    impl std::task::Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}
