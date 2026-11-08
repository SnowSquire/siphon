//! Slot arena and per-spool state.
//!
//! Two fixed slabs, both sized by construction:
//!
//! - **Slots**: fixed 192 KiB zeroed buffers, 32 per arena, in lazily chained
//!   arenas. A byte is initialized from the moment its arena exists, so the
//!   uninitialized-read class of problems cannot happen; slots are recycled
//!   without re-zeroing.
//! - **Spools**: one small record per live spool (a segment's byte stream:
//!   `refs`, `closed`, `published`), taken from a free list. A live spool owns at
//!   least one slot, so `MAX_SLOTS` entries are always enough.
//!
//! Everything is `Cell`/`RefCell`: the reactor is single threaded, so this is
//! `!Send` by construction and needs no atomics or locks.

use std::alloc::Layout;
use std::array;
use std::cell::Cell;
use std::cell::Ref;
use std::cell::RefCell;
use std::cell::RefMut;
use std::ops::Range;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::constants::ARENA_ALIGN;
use crate::constants::ARENA_BYTES;
use crate::constants::MAX_ARENAS;
use crate::constants::MAX_SLOTS;
use crate::constants::SLOT_SIZE;
use crate::constants::SLOTS_PER_ARENA;

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

/// Identifies a spool record inside a [`ArenaSet`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SpoolId(u32);

/// The arena has no free slot, or no free spool record, left.
///
/// Callers should back off and retry: a consumer is lagging behind the live
/// window.
#[derive(Debug, thiserror::Error)]
#[error("slot arena is exhausted")]
pub struct ArenaFull;

struct SlotMeta {
    /// Live owners: writer tail, consumer pins, and the predecessor's link.
    refs: Cell<usize>,
    /// Published bytes, monotonic, `0..=SLOT_SIZE`.
    published: Cell<usize>,
    /// Next slot of the same spool. The link itself holds one reference.
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

struct SpoolSlot {
    /// Writer plus every reader.
    refs: Cell<u32>,
    closed: Cell<bool>,
    /// Bytes published for the whole spool.
    published: Cell<usize>,
    /// Only meaningful while the record is on the free list.
    next_free: Cell<Option<SpoolId>>,
}

impl SpoolSlot {
    const fn new() -> Self {
        Self {
            refs: Cell::new(0),
            closed: Cell::new(false),
            published: Cell::new(0),
            next_free: Cell::new(None),
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
        // SAFETY: `start` is in bounds for the allocation `base` is derived from.
        let ptr = unsafe { self.base.as_ptr().add(start) };
        // SAFETY: `base` points to `SLOTS_PER_ARENA * SLOT_SIZE` bytes owned by the
        // arena and initialized by its construction (slots are never shrunk), and
        // `index < SLOTS_PER_ARENA` plus `published <= SLOT_SIZE` keep
        // `start .. start + published` in bounds. The range is at or below the
        // published frontier, which only ever gets shared views, so it is disjoint
        // from the writer's exclusive tail.
        unsafe { std::slice::from_raw_parts(ptr, published) }
    }

    /// Pointer to the unpublished tail `[published, SLOT_SIZE)` of slot `index`.
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

/// Slot storage and spool records, shared by everything on the reactor.
pub struct ArenaSet {
    arenas: RefCell<[Option<Rc<Arena>>; MAX_ARENAS]>,
    live: Cell<usize>,
    spools: RefCell<Box<[SpoolSlot]>>,
    free_spools: Cell<Option<SpoolId>>,
}

impl ArenaSet {
    #[must_use]
    pub fn new() -> Self {
        let spools: Box<[SpoolSlot]> = (0..MAX_SLOTS).map(|_| SpoolSlot::new()).collect();
        // Chain every record onto the free list once, at startup.
        for index in 0..MAX_SLOTS {
            let next = if index + 1 < MAX_SLOTS {
                Some(SpoolId(
                    u32::try_from(index + 1).expect("spool id fits in u32"),
                ))
            } else {
                None
            };
            spools[index].next_free.set(next);
        }

        Self {
            arenas: RefCell::new(array::from_fn(|_| None)),
            live: Cell::new(0),
            spools: RefCell::new(spools),
            free_spools: Cell::new(Some(SpoolId(0))),
        }
    }

    /// Allocate a zeroed slot with a refcount of one.
    ///
    /// Returns `None` at [`MAX_SLOTS`]; callers are expected to back off.
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
            let index = arenas.iter().position(Option::is_none)?;
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
        let arena = arenas[usize::from(id.arena)]
            .as_ref()
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

    /// Number of allocated slots across all broadcasts.
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

    // ------------------------------------------------------------- spools --

    fn spool_slot(&self, id: SpoolId) -> Ref<'_, SpoolSlot> {
        Ref::map(self.spools.borrow(), |spools| &spools[id.0 as usize])
    }

    fn spool_slot_mut(&self, id: SpoolId) -> RefMut<'_, SpoolSlot> {
        RefMut::map(self.spools.borrow_mut(), |spools| {
            &mut spools[id.0 as usize]
        })
    }

    /// Take a free spool record with one reference for the writer.
    #[must_use]
    pub fn alloc_spool(&self) -> Option<SpoolId> {
        let id = self.free_spools.get()?;
        let slot = self.spool_slot_mut(id);
        self.free_spools.set(slot.next_free.take());
        slot.refs.set(1);
        slot.closed.set(false);
        slot.published.set(0);
        Some(id)
    }

    pub fn retain_spool(&self, id: SpoolId) {
        let slot = self.spool_slot(id);
        let refs = slot.refs.get();
        debug_assert!(refs > 0, "retain of a free spool");
        slot.refs.set(refs + 1);
    }

    pub fn release_spool(&self, id: SpoolId) {
        let slot = self.spool_slot_mut(id);
        let refs = slot.refs.get();
        debug_assert!(refs > 0, "release of a free spool");
        let refs = refs - 1;
        slot.refs.set(refs);
        if refs == 0 {
            slot.next_free.set(self.free_spools.get());
            self.free_spools.set(Some(id));
        }
    }

    /// Bytes published for the whole spool: the change signal readers wait on.
    #[must_use]
    pub fn spool_published(&self, id: SpoolId) -> usize {
        self.spool_slot(id).published.get()
    }

    pub fn publish_spool(&self, id: SpoolId, published: usize) {
        let slot = self.spool_slot(id);
        debug_assert!(published >= slot.published.get(), "publish is monotonic");
        slot.published.set(published);
    }

    #[must_use]
    pub fn spool_closed(&self, id: SpoolId) -> bool {
        self.spool_slot(id).closed.get()
    }

    pub fn close_spool(&self, id: SpoolId) {
        self.spool_slot(id).closed.set(true);
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

        let spools = self.spools.borrow();
        let mut free_seen = 0usize;
        let mut id = self.free_spools.get();
        while let Some(spool) = id {
            free_seen += 1;
            assert_eq!(
                spools[spool.0 as usize].refs.get(),
                0,
                "free spool with refs"
            );
            id = spools[spool.0 as usize].next_free.get();
            assert!(free_seen <= MAX_SLOTS, "free list loop");
        }
    }
}

impl Default for ArenaSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Exclusive write access to a slot, owned by the spool writer.
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

/// Read cursor over one slot, owned by a spool consumer.
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

    /// Range of newly published bytes, advancing the cursor past them.
    pub fn take_range(&mut self) -> Option<Range<usize>> {
        let published = self.arena.published(self.id.index);
        if published <= self.offset {
            return None;
        }
        let start = self.offset;
        self.offset = published;
        Some(start..published)
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

/// An owned, pinned, read-only view of a published slot range.
///
/// `IoBuf` requires `'static`, so the view owns its reference; handing it to
/// compio's `write_all`/`write_at` keeps the bytes alive (and pinned) until the
/// operation completes, and cancellation drops the view, releasing the pin.
pub struct SlotView {
    set: Rc<ArenaSet>,
    arena: Rc<Arena>,
    id: SlotId,
    range: Range<usize>,
}

impl SlotView {
    #[must_use]
    pub fn new(set: &Rc<ArenaSet>, id: SlotId, range: Range<usize>) -> Self {
        set.retain(id);
        Self {
            arena: set.arena(id),
            set: Rc::clone(set),
            id,
            range,
        }
    }

    #[must_use]
    pub const fn id(&self) -> SlotId {
        self.id
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.range.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }
}

impl compio::buf::IoBuf for SlotView {
    fn as_init(&self) -> &[u8] {
        // `range` is inside the published prefix of a slot this view pins, so the
        // bytes are initialized and stay immutable until the pin is released.
        let bytes = self.arena.bytes(self.id.index, self.range.end);
        &bytes[self.range.clone()]
    }
}

impl Drop for SlotView {
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
        assert!(reader.take_range().is_none());

        writer.tail()[..4].copy_from_slice(b"live");
        writer.publish(4);
        assert_eq!(reader.take_range(), Some(0..4));
        assert!(reader.take_range().is_none());

        writer.tail()[..3].copy_from_slice(b"abc");
        writer.publish(7);
        assert_eq!(reader.take_range(), Some(4..7));
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

        assert_eq!(reader.take_range(), Some(0..2));
        assert!(reader.take_range().is_none());
        assert!(reader.advance());
        assert_eq!(reader.take_range(), Some(0..2));
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
        set.assert_invariants();
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
        set.assert_invariants();
    }

    #[test]
    fn spools_are_recycled_through_a_fixed_table() {
        let set = set();
        let first = set.alloc_spool().expect("spool");
        assert_eq!(set.spool_published(first), 0);
        assert!(!set.spool_closed(first));

        set.retain_spool(first);
        set.release_spool(first);
        assert_eq!(set.alloc_spool(), Some(SpoolId(first.0 + 1)));
        // Releasing the last reference on the *second* spool puts it back; the
        // first is still referenced.
        set.release_spool(first);
        assert!(set.alloc_spool().is_some());
        set.assert_invariants();
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
