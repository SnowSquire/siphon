//! A spool: the byte stream of one HLS segment.
//!
//! The downloader task appends into the spool's current slot; consumers hold a
//! [`Reader`] cursor and get owned, pinned [`SlotView`]s of the published bytes, so
//! TCP and disk writes hand arena memory straight to the kernel.
//!
//! ## Wakeups
//!
//! There is no wait list on the spool. Every consumer task owns one
//! [`ConsumerWake`] slot (one `Waker`, allocated once when the task starts) and
//! registers it in the scheduler's [`WakeSet`]. A publish wakes every registered
//! consumer; a consumer parked on a different spool re-checks its own frontier and
//! parks again, which costs one poll and keeps the per-spool state down to two
//! counters.

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
pub use crate::arena::SlotId;
use crate::arena::SlotMut;
pub use crate::arena::SlotRef;
pub use crate::arena::SlotView;
pub use crate::arena::SpoolId;
use crate::constants::SLOT_BUDGET;

/// One waker slot per consumer task.
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

    fn park(&self, context: &Context<'_>) {
        self.waker.set(Some(context.waker().clone()));
    }

    fn wake(&self) {
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
pub struct Spool;

impl Spool {
    /// Open a spool in `arena`, with its wakeups delivered through `wakes`.
    pub fn open(arena: &Rc<ArenaSet>, wakes: &Rc<WakeSet>) -> Result<(Writer, Reader), ArenaFull> {
        let id = arena.alloc_spool().ok_or(ArenaFull)?;
        let Some(head) = arena.alloc() else {
            arena.release_spool(id);
            return Err(ArenaFull);
        };

        let tail = SlotMut::adopt(arena, head);
        let reader = Reader::new(arena, id, head);

        Ok((
            Writer {
                arena: Rc::clone(arena),
                wakes: Rc::clone(wakes),
                id,
                tail,
                written: 0,
                slots: 1,
            },
            reader,
        ))
    }
}

/// The single writer of one spool.
pub struct Writer {
    arena: Rc<ArenaSet>,
    wakes: Rc<WakeSet>,
    id: SpoolId,
    tail: SlotMut,
    written: usize,
    /// Slots this spool owns, capped at the per-spool budget so one lagging spool
    /// cannot eat the global arena.
    slots: usize,
}

impl Writer {
    /// Writable remainder of the tail slot, linking a new slot when it is full.
    pub fn tail(&mut self) -> Result<&mut [u8], ArenaFull> {
        if self.tail.is_full() {
            if self.slots >= SLOT_BUDGET {
                return Err(ArenaFull);
            }
            let id = self.arena.alloc().ok_or(ArenaFull)?;
            let next = SlotMut::adopt(&self.arena, id);
            self.arena.link(self.tail.id(), id);
            self.tail = next;
            self.slots += 1;
        }
        Ok(self.tail.tail())
    }

    /// Publish `len` more bytes and wake the consumers.
    pub fn commit(&mut self, len: usize) {
        let published = self.tail.published() + len;
        self.tail.publish(published);
        self.written += len;
        self.arena.publish_spool(self.id, self.written);
        self.wakes.wake_all();
    }

    /// Nothing has been written yet, so a retry cannot corrupt the spool.
    pub const fn is_empty(&self) -> bool {
        self.written == 0
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.arena.close_spool(self.id);
        self.wakes.wake_all();
        self.arena.release_spool(self.id);
    }
}

/// A consumer's cursor over one spool.
pub struct Reader {
    arena: Rc<ArenaSet>,
    id: SpoolId,
    head: SlotId,
    cursor: SlotRef,
}

impl Reader {
    fn new(arena: &Rc<ArenaSet>, id: SpoolId, head: SlotId) -> Self {
        arena.retain_spool(id);
        Self {
            arena: Rc::clone(arena),
            id,
            head,
            cursor: SlotRef::new(arena, head),
        }
    }

    /// A fresh cursor at the first byte of the spool, for late joiners and for
    /// handing the same spool to another consumer.
    pub fn rewind(&self) -> Self {
        Self::new(&self.arena, self.id, self.head)
    }

    /// Next published run as an owned, pinned view. `None` once the writer is gone
    /// and everything has been consumed.
    ///
    /// `wake` is the caller's own slot; it is parked here while there is nothing to
    /// read.
    pub async fn next_view(&mut self, wake: &ConsumerWake) -> Option<SlotView> {
        loop {
            let seen = self.arena.spool_published(self.id);

            if let Some(range) = self.cursor.take_range() {
                return Some(SlotView::new(&self.arena, self.cursor.id(), range));
            }

            if self.arena.next(self.cursor.id()).is_some() {
                self.cursor.advance();
                continue;
            }

            if self.arena.spool_closed(self.id) {
                return None;
            }

            Wait {
                arena: &self.arena,
                id: self.id,
                wake,
                seen,
            }
            .await;
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.arena.release_spool(self.id);
    }
}

impl Clone for Reader {
    fn clone(&self) -> Self {
        self.rewind()
    }
}

#[allow(clippy::missing_fields_in_debug)] // the arena is a shared handle, not state
impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("head", &self.head)
            .field("cursor", &self.cursor.id())
            .finish()
    }
}

struct Wait<'a> {
    arena: &'a ArenaSet,
    id: SpoolId,
    wake: &'a ConsumerWake,
    /// Published length observed before checking the cursor.
    seen: usize,
}

impl Future for Wait<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        let changed =
            || self.arena.spool_closed(self.id) || self.arena.spool_published(self.id) != self.seen;
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
    fn a_clone_rewinds_to_the_start() {
        let arena = arena();
        let wakes = WakeSet::new();
        let wake = ConsumerWake::new();
        let (mut writer, reader) = Spool::open(&arena, &wakes).expect("spool");

        let (first_out, second_out) = run(async {
            writer.tail().expect("tail")[..3].copy_from_slice(b"abc");
            writer.commit(3);
            drop(writer);

            let mut first = reader.clone();
            let mut second = reader;
            let mut first_out = Vec::new();
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
        assert_eq!(second_out, b"abc");
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
