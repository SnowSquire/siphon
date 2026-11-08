//! Slot arena for segment chunk storage.
//!
//! Replaces the per-segment `Arc<Chunk>` chain (`Chunk::new`, `OnceLock<next>`,
//! recursive drop) with a flat arena of fixed-size, zero-initialized slots.
//!
//! Every slot byte is a valid `u8` from the moment the arena exists, so the whole
//! `MaybeUninit` / `assume_init` / "uninitialized read" class of problems is
//! structurally impossible rather than merely carefully avoided. Slots are recycled
//! without re-zeroing.
//!
//! The arena owns no threading or IO policy: it is a `Cell`/`Rc` structure intended
//! for the single-threaded reactor, so it is `!Send` and `!Sync` by construction and
//! needs no atomics or locks.
//!
//! # Reference counting
//!
//! `SlotMeta::refs` counts every owner of a slot:
//!
//! - the writer's current tail (one allocation),
//! - every consumer pin ([`SlotRef`]),
//! - the predecessor's `next` link (which is why a lagging reader can never follow
//!   a recycled slot).
//!
//! A slot returns to its arena's free bitmap when its refcount reaches zero, and
//! releasing the slot then releases its `next` link, so a chain is reclaimed from
//! the head forward, iteratively (never recursively).
//!
//! # Aliasing
//!
//! Published bytes (`[0, published)`) are immutable and only handed out as `&[u8]`;
//! the writer's [`SlotMut`] only hands out the unpublished tail
//! (`[published, SLOT_SIZE)`). The two ranges are disjoint by construction, so the
//! single `UnsafeCell` around the payload never produces overlapping `&`/`&mut`.

use std::alloc::Layout;
use std::array;
use std::cell::Cell;
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

/// Bytes per slot: `188 * 1024`.
///
/// 188 is the mpegts packet size, so a slot boundary never splits a packet; 1024
/// packets also makes it 4 KiB page aligned (`47` pages), so the one-time zeroing
/// maps to whole pages.
pub const SLOT_SIZE: usize = 192_512;

/// Slots per arena (one `u32` free bitmap).
pub const SLOTS_PER_ARENA: usize = 32;

/// Payload bytes of one arena.
const ARENA_BYTES: usize = SLOTS_PER_ARENA * SLOT_SIZE;

/// Alignment of the payload. Slots are a multiple of this, so each `SlotId` region
/// starts on a page boundary as well.
const ARENA_ALIGN: usize = 4096;

const ARENAS_PER_STREAM: usize = 6;

/// Slots a single stream may pin at once: six arenas, ~37 MB, i.e. 30 s of an
/// ~9.8 Mbps stream (Twitch lists 30 s of segments in the live playlist, and the
/// scheduler's replay cursors pin that whole window).
pub const SLOTS_PER_STREAM: usize = ARENAS_PER_STREAM * SLOTS_PER_ARENA;

/// Streams the daemon may serve concurrently.
pub const MAX_STREAMS: usize = 4;

/// Hard ceiling across all streams. Arenas are allocated lazily, so this is a
/// ceiling and not a reservation.
pub const MAX_ARENAS: usize = ARENAS_PER_STREAM * MAX_STREAMS;

/// Total slots across all streams.
pub const MAX_SLOTS: usize = MAX_ARENAS * SLOTS_PER_ARENA;

/// Identifies a slot inside a [`ArenaSet`].
///
/// Ids are stable while the slot is referenced; because an arena is only released
/// when it has no live slots, an id can never observe a recycled arena index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SlotId {
    arena: u16,
    index: u8,
}

impl SlotId {
    const fn new(arena: u16, index: u8) -> Self {
        Self { arena, index }
    }

    /// Index of the owning arena within the set.
    #[must_use]
    pub const fn arena(self) -> u16 {
        self.arena
    }

    /// Index of the slot within its arena.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.index
    }
}

struct SlotMeta {
    /// Live owners: writer allocation, consumer pins, and the predecessor's link.
    refs: Cell<usize>,
    /// Published bytes, monotonic, `0..=SLOT_SIZE`.
    published: Cell<usize>,
    /// Next slot of the same segment. The link itself holds one reference.
    next: Cell<Option<SlotId>>,
}

impl SlotMeta {
    const fn new() -> Self {
        Self {
            refs: Cell::new(0),
            published: Cell::new(0),
            next: Cell::new(None),
        }
    }
}

struct Arena {
    /// `ARENA_BYTES` zero-initialized bytes, obtained with `alloc_zeroed` so
    /// untouched pages are never dirtied. Deliberately not a `Box<[u8]>`: moving
    /// the box would retag its contents and invalidate every pointer derived from
    /// it, while this raw pointer is the only handle to the allocation.
    base: NonNull<u8>,
    slots: [SlotMeta; SLOTS_PER_ARENA],
    /// Bit set => slot is free.
    free: Cell<u32>,
    /// Slots with `refs > 0`.
    used: Cell<usize>,
}

impl Drop for Arena {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(ARENA_BYTES, ARENA_ALIGN).expect("valid arena layout");
        // SAFETY: `base` was allocated in `Arena::new` with exactly this layout and
        // has not been freed or reallocated since.
        unsafe { std::alloc::dealloc(self.base.as_ptr(), layout) };
    }
}

impl Arena {
    fn new() -> Self {
        let layout = Layout::from_size_align(ARENA_BYTES, ARENA_ALIGN).expect("valid arena layout");
        // SAFETY: the layout has a non-zero size, so `alloc_zeroed` either returns
        // a non-null pointer to `ARENA_BYTES` zeroed, `ARENA_ALIGN`-aligned bytes
        // or null, which is turned into an allocation failure below.
        let base = NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })
            .unwrap_or_else(|| std::alloc::handle_alloc_error(layout));
        Self {
            base,
            slots: array::from_fn(|_| SlotMeta::new()),
            free: Cell::new(u32::MAX),
            used: Cell::new(0),
        }
    }

    /// Take a free slot, returning its index. The slot is handed out with a
    /// refcount of one, owned by the caller.
    fn take_free(&self) -> Option<u8> {
        let free = self.free.get();
        if free == 0 {
            return None;
        }
        let index = u8::try_from(free.trailing_zeros()).expect("bit index fits in u8");
        self.free.set(free & !(1 << index));

        let slot = &self.slots[usize::from(index)];
        debug_assert_eq!(slot.refs.get(), 0, "free slot with live refs");
        slot.refs.set(1);
        slot.published.set(0);
        slot.next.set(None);

        self.used.set(self.used.get() + 1);
        Some(index)
    }

    fn slot(&self, index: u8) -> &SlotMeta {
        &self.slots[usize::from(index)]
    }

    fn retain(&self, index: u8) {
        let slot = self.slot(index);
        let refs = slot.refs.get();
        debug_assert!(refs > 0, "retain of a free slot");
        slot.refs.set(refs + 1);
    }

    /// Returns `true` when the refcount reached zero and the slot went back to the
    /// free bitmap.
    fn release(&self, index: u8) -> bool {
        let slot = self.slot(index);
        let refs = slot.refs.get();
        debug_assert!(refs > 0, "release of a free slot");
        let refs = refs - 1;
        slot.refs.set(refs);
        if refs > 0 {
            return false;
        }

        slot.published.set(0);
        self.used.set(self.used.get() - 1);
        self.free.set(self.free.get() | (1 << index));
        true
    }

    fn next(&self, index: u8) -> Option<SlotId> {
        self.slot(index).next.get()
    }

    fn take_next(&self, index: u8) -> Option<SlotId> {
        self.slot(index).next.take()
    }

    fn published(&self, index: u8) -> usize {
        self.slot(index).published.get()
    }

    fn publish(&self, index: u8, len: usize) {
        let slot = self.slot(index);
        let published = slot.published.get();
        assert!(
            len >= published && len <= SLOT_SIZE,
            "publish must be monotonic and inside the slot"
        );
        slot.published.set(len);
    }

    /// Published bytes `[0, published)` of slot `index`.
    fn bytes(&self, index: u8, published: usize) -> &[u8] {
        let start = usize::from(index) * SLOT_SIZE;
        debug_assert!(published <= SLOT_SIZE);
        // SAFETY: `start` is in bounds for the allocation `base` is derived from,
        // see `tail_ptr`.
        let ptr = unsafe { self.base.as_ptr().add(start) };
        // SAFETY: `base` points to `SLOTS_PER_ARENA * SLOT_SIZE` bytes owned by
        // `_data` and initialized by the arena's construction (slots are never
        // shrunk), and `index < SLOTS_PER_ARENA` plus `published <= SLOT_SIZE`
        // keep `start .. start + published` in bounds. The range is at or below
        // the published frontier, which only ever gets shared views, so it is
        // disjoint from the writer's exclusive tail.
        unsafe { std::slice::from_raw_parts(ptr, published) }
    }

    /// Pointer to the unpublished tail `[published, SLOT_SIZE)` of slot `index`.
    ///
    /// A raw pointer is returned because the lifetime has to be pinned to the
    /// guard, not to the `&self` borrow of the arena.
    fn tail_ptr(&self, index: u8, published: usize) -> *mut u8 {
        let start = usize::from(index) * SLOT_SIZE + published;
        debug_assert!(published <= SLOT_SIZE);
        // SAFETY: `start` is at most `ARENA_BYTES` by the bounds above, so the
        // offset stays inside the allocation `base` was derived from. The pointer is
        // only dereferenced by `SlotMut`, which holds the slot's single writer
        // reference.
        unsafe { self.base.as_ptr().add(start) }
    }
}

/// Set of arenas, shared by every stream of the daemon.
pub struct ArenaSet {
    arenas: RefCell<Vec<Option<Rc<Arena>>>>,
    live: Cell<usize>,
}

impl ArenaSet {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            arenas: RefCell::new(Vec::new()),
            live: Cell::new(0),
        }
    }

    /// Allocate a zeroed slot with a refcount of one.
    ///
    /// Returns `None` at [`MAX_SLOTS`]; callers are expected to evict a lagging
    /// consumer first.
    #[must_use]
    pub fn alloc(&self) -> Option<SlotId> {
        {
            let arenas = self.arenas.borrow();
            for (index, arena) in arenas.iter().enumerate() {
                let Some(arena) = arena else { continue };
                if let Some(slot) = arena.take_free() {
                    self.live.set(self.live.get() + 1);
                    let index = u16::try_from(index).expect("arena index fits in u16");
                    return Some(SlotId::new(index, slot));
                }
            }
        }

        let (index, arena) = {
            let mut arenas = self.arenas.borrow_mut();
            let index = if let Some(index) = arenas.iter().position(Option::is_none) {
                index
            } else {
                if arenas.len() == MAX_ARENAS {
                    return None;
                }
                arenas.push(None);
                arenas.len() - 1
            };
            let arena = Rc::new(Arena::new());
            arenas[index] = Some(Rc::clone(&arena));
            (index, arena)
        };

        let slot = arena.take_free().expect("a fresh arena has free slots");
        self.live.set(self.live.get() + 1);
        let index = u16::try_from(index).expect("arena index fits in u16");
        Some(SlotId::new(index, slot))
    }

    fn arena(&self, id: SlotId) -> Rc<Arena> {
        let arenas = self.arenas.borrow();
        let arena = arenas
            .get(usize::from(id.arena))
            .and_then(Option::as_ref)
            .expect("slot id refers to a freed arena");
        Rc::clone(arena)
    }

    /// Add a reference to `id`.
    pub fn retain(&self, id: SlotId) {
        self.arena(id).retain(id.index);
    }

    /// Drop a reference to `id`, releasing its `next` link and the arena itself
    /// when it reaches zero.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when releasing a slot that is already free.
    pub fn release(&self, id: SlotId) {
        let mut current = Some(id);
        while let Some(id) = current {
            let arena = self.arena(id);
            if !arena.release(id.index) {
                break;
            }
            self.live.set(self.live.get() - 1);
            // Iterative, never recursive: chains are bounded by `MAX_SLOTS` but a
            // recursive drop would still be a stack-depth liability.
            current = arena.take_next(id.index);
            if arena.used.get() == 0 {
                self.drop_arena(id.arena);
            }
        }
    }

    fn drop_arena(&self, index: u16) {
        let mut arenas = self.arenas.borrow_mut();
        if let Some(entry) = arenas.get_mut(usize::from(index)) {
            *entry = None;
        }
    }

    /// Link `from -> to`. The link holds its own reference to `to`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when `from` is already linked.
    pub fn link(&self, from: SlotId, to: SlotId) {
        let arena = self.arena(from);
        debug_assert!(
            arena.next(from.index).is_none(),
            "a slot is linked exactly once"
        );
        // Take the link's reference before publishing the pointer.
        self.retain(to);
        arena.slot(from.index).next.set(Some(to));
    }

    /// Next slot of the chain, if the link exists.
    #[must_use]
    pub fn next(&self, id: SlotId) -> Option<SlotId> {
        self.arena(id).next(id.index)
    }

    /// Published bytes of `id`.
    #[must_use]
    pub fn published(&self, id: SlotId) -> usize {
        self.arena(id).published(id.index)
    }

    /// Number of allocated slots across all streams.
    #[must_use]
    pub const fn live_slots(&self) -> usize {
        self.live.get()
    }

    /// Number of arenas currently allocated.
    #[must_use]
    pub fn arena_count(&self) -> usize {
        self.arenas.borrow().iter().filter(|a| a.is_some()).count()
    }

    /// Number of live references to `id`. Debug aid.
    #[must_use]
    pub fn refs(&self, id: SlotId) -> usize {
        self.arena(id).slot(id.index).refs.get()
    }

    /// Debug-only consistency check: free bitmaps, per-arena `used` counters and
    /// the global live counter must agree with the per-slot refcounts.
    #[cfg(debug_assertions)]
    pub fn assert_invariants(&self) {
        let arenas = self.arenas.borrow();
        let mut live = 0;

        for arena in arenas.iter().flatten() {
            let mut used = 0;
            for (index, slot) in arena.slots.iter().enumerate() {
                let is_free = (arena.free.get() >> index) & 1 == 1;
                assert_eq!(
                    is_free,
                    slot.refs.get() == 0,
                    "free bitmap disagrees with refcounts"
                );
                assert!(slot.published.get() <= SLOT_SIZE, "published past the slot");
                if slot.refs.get() > 0 {
                    used += 1;
                    live += 1;
                }
            }
            assert_eq!(used, arena.used.get(), "arena `used` counter out of sync");
        }

        assert_eq!(live, self.live.get(), "global live counter out of sync");
    }
}

impl Default for ArenaSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Exclusive write access to a slot, owned by the segment writer.
///
/// Obtained from [`ArenaSet::alloc`] and holds that allocation's single reference
/// until dropped.
pub struct SlotMut {
    set: Rc<ArenaSet>,
    arena: Rc<Arena>,
    id: SlotId,
}

impl SlotMut {
    /// Adopt the reference returned by [`ArenaSet::alloc`].
    #[must_use]
    pub fn adopt(set: &Rc<ArenaSet>, id: SlotId) -> Self {
        Self {
            arena: set.arena(id),
            set: Rc::clone(set),
            id,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SlotId {
        self.id
    }

    /// Published bytes of this slot.
    #[must_use]
    pub fn published(&self) -> usize {
        self.arena.published(self.id.index)
    }

    /// Unwritten remainder of this slot.
    ///
    /// This is the only writable view of the slot, so it can never alias a
    /// reader's published range.
    pub fn tail(&mut self) -> &mut [u8] {
        let published = self.arena.published(self.id.index);
        let len = SLOT_SIZE - published;
        // SAFETY: `SlotMut` is the slot's single writer reference, so no other
        // reference to `[published, SLOT_SIZE)` exists: readers only see
        // `[0, published)`. The lifetime is tied to `&mut self`, which keeps the
        // guard (and therefore the arena) alive.
        unsafe {
            std::slice::from_raw_parts_mut(self.arena.tail_ptr(self.id.index, published), len)
        }
    }

    /// Publish `len` bytes of this slot, making `[0, len)` visible to readers.
    ///
    /// # Panics
    ///
    /// Panics when `len` decreases or exceeds [`SLOT_SIZE`].
    pub fn publish(&self, len: usize) {
        self.arena.publish(self.id.index, len);
    }

    #[must_use]
    pub fn is_full(&self) -> bool {
        self.published() == SLOT_SIZE
    }
}

impl Drop for SlotMut {
    fn drop(&mut self) {
        self.set.release(self.id);
    }
}

/// Read cursor over one slot, owned by a segment consumer.
///
/// Holds a reference to the slot until dropped.
pub struct SlotRef {
    set: Rc<ArenaSet>,
    arena: Rc<Arena>,
    id: SlotId,
    offset: usize,
}

impl SlotRef {
    /// Pin `id` and start a cursor at its beginning.
    #[must_use]
    pub fn new(set: &Rc<ArenaSet>, id: SlotId) -> Self {
        set.retain(id);
        Self {
            arena: set.arena(id),
            set: Rc::clone(set),
            id,
            offset: 0,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SlotId {
        self.id
    }

    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Next published run of the current slot, or `None` when none is available
    /// yet. Advances the cursor to the end of the returned run.
    pub fn chunk(&mut self) -> Option<&[u8]> {
        let published = self.arena.published(self.id.index);
        if published <= self.offset {
            return None;
        }
        let start = self.offset;
        self.offset = published;
        Some(&self.arena.bytes(self.id.index, published)[start..])
    }

    /// Move the cursor to the next slot of the chain.
    ///
    /// The next slot is pinned before the current one is released, so a chain can
    /// never be reclaimed from under the cursor.
    ///
    /// Returns `false` when the chain ends here.
    pub fn advance(&mut self) -> bool {
        let Some(next) = self.arena.next(self.id.index) else {
            return false;
        };
        self.set.retain(next);
        self.set.release(self.id);
        self.id = next;
        self.arena = self.set.arena(next);
        self.offset = 0;
        true
    }
}

impl Drop for SlotRef {
    fn drop(&mut self) {
        self.set.release(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> Rc<ArenaSet> {
        Rc::new(ArenaSet::new())
    }

    #[test]
    fn slots_are_full_size_and_zeroed() {
        let set = set();
        let mut writer = SlotMut::adopt(&set, set.alloc().expect("slot"));
        assert_eq!(writer.tail().len(), SLOT_SIZE);
        assert!(writer.tail().iter().all(|&byte| byte == 0));
        assert_eq!(set.live_slots(), 1);
    }

    #[test]
    fn publish_is_visible_to_readers_incrementally() {
        let set = set();
        let id = set.alloc().expect("slot");
        let mut writer = SlotMut::adopt(&set, id);

        let mut reader = SlotRef::new(&set, id);
        assert_eq!(reader.chunk(), None);

        writer.tail()[..4].copy_from_slice(b"live");
        writer.publish(4);
        assert_eq!(reader.chunk(), Some(&b"live"[..]));
        assert_eq!(reader.chunk(), None);

        writer.tail()[..3].copy_from_slice(b"abc");
        writer.publish(7);
        assert_eq!(reader.chunk(), Some(&b"abc"[..]));
        assert!(!writer.is_full());
    }

    #[test]
    fn refcounts_delay_recycling() {
        let set = set();
        let id = set.alloc().expect("slot");
        let pin = SlotRef::new(&set, id);
        drop(SlotMut::adopt(&set, id));

        assert_eq!(set.live_slots(), 1);
        assert_eq!(set.refs(id), 1);

        drop(pin);
        assert_eq!(set.live_slots(), 0);
        assert_eq!(set.arena_count(), 0);

        // The bitmap hands the same slot back.
        assert_eq!(set.alloc(), Some(id));
    }

    #[test]
    fn linked_slots_hold_a_reference() {
        let set = set();
        let a = set.alloc().expect("slot");
        let b = set.alloc().expect("slot");

        set.link(a, b);
        assert_eq!(set.refs(b), 2);

        // Releasing the writer's reference to `a` cascades into `b`, which keeps
        // its own reference.
        set.release(a);
        assert_eq!(set.live_slots(), 1);

        set.release(b);
        assert_eq!(set.live_slots(), 0);
        assert_eq!(set.arena_count(), 0);
    }

    #[test]
    fn readers_walk_the_chain_and_pin_what_they_hold() {
        let set = set();
        let a = set.alloc().expect("slot");
        let b = set.alloc().expect("slot");

        let mut writer_a = SlotMut::adopt(&set, a);
        let mut reader = SlotRef::new(&set, a);
        writer_a.tail()[..2].copy_from_slice(b"aa");
        writer_a.publish(2);
        set.link(a, b);
        drop(writer_a);

        let mut writer_b = SlotMut::adopt(&set, b);
        writer_b.tail()[..2].copy_from_slice(b"bb");
        writer_b.publish(2);

        assert_eq!(reader.chunk(), Some(&b"aa"[..]));
        assert_eq!(reader.chunk(), None);
        assert!(reader.advance());
        assert_eq!(reader.chunk(), Some(&b"bb"[..]));
        assert!(!reader.advance());

        drop(reader);
        drop(writer_b);
        assert_eq!(set.live_slots(), 0);
        set.assert_invariants();
    }

    #[test]
    fn arenas_are_freed_when_empty_and_reused() {
        let set = set();
        let ids: Vec<SlotId> = (0..SLOTS_PER_ARENA)
            .map(|_| set.alloc().expect("slot"))
            .collect();
        assert_eq!(set.arena_count(), 1);
        assert_eq!(set.live_slots(), SLOTS_PER_ARENA);

        for id in ids {
            set.release(id);
        }
        assert_eq!(set.arena_count(), 0);
        assert_eq!(set.live_slots(), 0);

        let id = set.alloc().expect("slot");
        assert_eq!(set.arena_count(), 1);
        assert_eq!(id.index(), 0);
    }

    #[test]
    fn allocation_stops_at_the_global_cap() {
        let set = set();
        let ids: Vec<SlotId> = std::iter::from_fn(|| set.alloc()).collect();
        assert_eq!(ids.len(), MAX_SLOTS);
        assert_eq!(set.arena_count(), MAX_ARENAS);
        assert_eq!(set.alloc(), None);

        set.release(ids[0]);
        assert!(set.alloc().is_some());
        set.assert_invariants();
    }

    #[test]
    fn slots_may_span_multiple_arenas() {
        let set = set();
        let ids: Vec<SlotId> = (0..=SLOTS_PER_ARENA)
            .map(|_| set.alloc().expect("slot"))
            .collect();
        assert_eq!(set.arena_count(), 2);
        assert_eq!(ids[SLOTS_PER_ARENA].arena(), 1);

        for id in ids {
            set.release(id);
        }
        assert_eq!(set.arena_count(), 0);
    }

    #[test]
    #[should_panic(expected = "release of a free slot")]
    fn double_release_panics() {
        let set = set();
        let keep = set.alloc().expect("slot");
        let id = set.alloc().expect("slot");
        set.release(id);
        set.release(id);
        let _ = keep;
    }

    #[test]
    #[should_panic(expected = "retain of a free slot")]
    fn retain_after_release_panics() {
        let set = set();
        let keep = set.alloc().expect("slot");
        let id = set.alloc().expect("slot");
        set.release(id);
        set.retain(id);
        let _ = keep;
    }

    #[test]
    #[should_panic(expected = "slot id refers to a freed arena")]
    fn release_after_the_arena_is_freed_panics() {
        let set = set();
        let id = set.alloc().expect("slot");
        set.release(id);
        assert_eq!(set.arena_count(), 0);
        set.release(id);
    }
}
