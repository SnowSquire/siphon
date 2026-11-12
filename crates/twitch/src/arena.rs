//! Slot arena: the fixed-size zeroed buffers every spool is written into.
//!
//! - **Slots** are [`SLOT_SIZE`] zeroed buffers, [`SLOTS_PER_ARENA`] of them per
//!   arena. A byte is initialized from the moment its arena exists, so the
//!   uninitialized-read class of problems cannot happen; slots are recycled
//!   without re-zeroing.
//! - **Arenas** are heap nodes in an intrusive singly linked list, so a middle
//!   node can be dropped without a table, and [`SlotId`] carries the arena
//!   pointer, so there is no id-to-arena lookup either. Splicing a middle node
//!   walks to its predecessor, but that only happens when an arena empties (once
//!   per [`SLOTS_PER_ARENA`] slot frees); [`ArenaSet::alloc`] only takes the head.
//! - **Search**: a second intrusive list (the *free* list) holds exactly the
//!   arenas with at least one free slot, so [`ArenaSet::alloc`] is O(1) and never
//!   scans.
//! - **The spare**: an emptied arena is kept, empty, as the single spare and is
//!   consumed by the first allocation the free list cannot satisfy. Without it
//!   a pool hovering at an arena boundary would allocate and free a zeroed
//!   [`ARENA_BYTES`] arena per slot.
//! - **No global cap**: failing to allocate an arena surfaces as [`ArenaFull`],
//!   exactly like the per-spool slot cap, so a memory squeeze fails one segment
//!   instead of aborting the process.
//!
//! Everything is `Cell`: the reactor is single threaded, so this is `!Send` by
//! construction and needs no atomics or locks.

use std::alloc::Layout;
use std::array;
use std::cell::Cell;
use std::fmt;
use std::ops::Range;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::constants::ARENA_ALIGN;
use crate::constants::ARENA_BYTES;
use crate::constants::SLOT_SIZE;
use crate::constants::SLOTS_PER_ARENA;

/// The spool reached its slot cap, or the system could not give us another
/// arena.
///
/// Callers should back off and retry: a consumer is lagging behind the live
/// window.
#[derive(Debug, thiserror::Error)]
#[error("slot arena is exhausted")]
pub struct ArenaFull;

/// Identifies one live slot of an [`ArenaSet`].
///
/// The arena is identified by its address, so going from a slot to its arena
/// needs no table. A live id always points at a live arena: an arena is only
/// freed once its last slot is released, and this id's slot is still referenced.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId {
    arena: NonNull<Arena>,
    index: u8,
}

impl SlotId {
    /// Index of the slot within its arena.
    #[must_use]
    pub const fn index(self) -> u8 {
        self.index
    }

    /// The arena this slot belongs to.
    ///
    /// Borrowed from `&self`: holding the id is what keeps the arena alive, so the
    /// returned reference is valid for as long as the caller uses the id.
    const fn owner(&self) -> &Arena {
        // SAFETY: a live id's slot holds a reference, and an arena is freed only
        // when its last slot is released, so the arena behind it is still there.
        unsafe { self.arena.as_ref() }
    }

    /// Next slot of the chain, if the link exists.
    #[must_use]
    pub(crate) fn next(self) -> Option<Self> {
        self.owner().next(self.index)
    }

    /// Published bytes of this slot.
    #[must_use]
    fn published(self) -> usize {
        self.owner().published(self.index)
    }

    /// Number of live references. Debug aid.
    #[must_use]
    fn refs(self) -> usize {
        self.owner().slot(self.index).refs.get()
    }

    /// Add a reference.
    fn retain(self) {
        self.owner().retain(self.index);
    }

    /// Link `self -> next`. The link holds its own reference to `next`.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when `self` is already linked.
    pub(crate) fn link(self, next: Self) {
        debug_assert!(self.next().is_none(), "a slot is linked exactly once");
        // Take the link's reference before publishing the pointer.
        next.retain();
        self.owner().slot(self.index).next.set(Some(next));
    }
}

impl fmt::Debug for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `NonNull`'s own `Debug` prints the address, which is all an id has.
        f.debug_struct("SlotId")
            .field("arena", &self.arena.as_ptr())
            .field("index", &self.index)
            .finish()
    }
}

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

    /// Next arena in the list of every allocated arena.
    next: Cell<Option<NonNull<Self>>>,
    /// Next arena in the free list, and whether this arena is on it.
    free_next: Cell<Option<NonNull<Self>>>,
    on_free_list: Cell<bool>,
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
    /// A fresh arena, or `None` when the allocator refuses.
    ///
    /// Returning `None` instead of calling `handle_alloc_error` is what keeps the
    /// pool uncapped: the failure becomes one failed segment, not a dead process.
    fn new() -> Option<Self> {
        let layout = Layout::from_size_align(ARENA_BYTES, ARENA_ALIGN).expect("valid arena layout");
        // SAFETY: the layout has a non-zero size, so `alloc_zeroed` either returns
        // a non-null pointer to `ARENA_BYTES` zeroed, `ARENA_ALIGN`-aligned bytes
        // or null.
        let base = NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })?;
        Some(Self {
            base,
            slots: array::from_fn(|_| SlotMeta::new()),
            free: Cell::new(u32::MAX),
            used: Cell::new(0),
            next: Cell::new(None),
            free_next: Cell::new(None),
            on_free_list: Cell::new(false),
        })
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

        let slot = self.slot(index);
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

/// Slot storage, shared by every spool on the reactor.
pub struct ArenaSet {
    /// Every allocated arena, spare included.
    arenas: Cell<Option<NonNull<Arena>>>,
    /// The arenas with at least one free slot, most recently used first.
    free_list: Cell<Option<NonNull<Arena>>>,
    /// One empty arena, kept out of the allocation path.
    spare: Cell<Option<NonNull<Arena>>>,
    /// Allocated arenas, spare included.
    count: Cell<usize>,
    /// Slots with a live reference.
    live: Cell<usize>,
}

impl ArenaSet {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            arenas: Cell::new(None),
            free_list: Cell::new(None),
            spare: Cell::new(None),
            count: Cell::new(0),
            live: Cell::new(0),
        }
    }

    /// Allocate a zeroed slot with a refcount of one.
    ///
    /// Returns `None` when the system cannot give us another arena; callers are
    /// expected to back off.
    #[must_use]
    pub fn alloc(&self) -> Option<SlotId> {
        let arena = match self.pop_free() {
            Some(arena) => arena,
            None => self.grow()?,
        };

        // SAFETY: `arena` is a live node of the list.
        let node = unsafe { arena.as_ref() };
        let index = node
            .take_free()
            .expect("an arena on the free list has a free slot");
        self.live.set(self.live.get() + 1);
        if node.free.get() != 0 {
            self.push_free(arena);
        }
        Some(SlotId { arena, index })
    }

    /// Take the spare, or allocate a new arena.
    fn grow(&self) -> Option<NonNull<Arena>> {
        if let Some(spare) = self.spare.take() {
            return Some(spare);
        }
        let arena = NonNull::from(Box::leak(Box::new(Arena::new()?)));
        self.push_arena(arena);
        self.count.set(self.count.get() + 1);
        Some(arena)
    }

    /// Drop a reference to `id`, cascading into its `next` link and releasing the
    /// arena when it empties.
    ///
    /// # Panics
    ///
    /// Panics in debug builds when releasing a slot that is already free.
    pub fn release(&self, id: SlotId) {
        let mut current = Some(id);
        while let Some(id) = current {
            // SAFETY: the id is live until this loop releases its last reference,
            // and the link below keeps the next arena alive.
            let arena = unsafe { id.arena.as_ref() };
            if !arena.release(id.index) {
                // Another reference still holds the slot.
                break;
            }
            self.live.set(self.live.get() - 1);
            // Read the link before the arena can be freed.
            current = arena.take_next(id.index);
            if arena.used.get() == 0 {
                self.retire(id.arena);
            } else {
                self.push_free(id.arena);
            }
        }
    }

    /// An arena just emptied: keep it as the spare, or free it.
    fn retire(&self, arena: NonNull<Arena>) {
        debug_assert_ne!(self.spare.get(), Some(arena), "the spare holds no slots");
        self.remove_free(arena);
        if let Some(previous) = self.spare.replace(Some(arena)) {
            self.free_arena(previous);
        }
    }

    /// Unlink `arena` from both lists and free it.
    fn free_arena(&self, arena: NonNull<Arena>) {
        self.remove_free(arena);
        self.remove_arena(arena);
        self.count.set(self.count.get() - 1);
        // SAFETY: the node came from `Box::leak` in `grow`, is not the spare, and
        // no live slot or list link refers to it any more.
        unsafe { drop(Box::from_raw(arena.as_ptr())) };
    }

    /// Prepend `arena` to the list of all allocated arenas.
    fn push_arena(&self, arena: NonNull<Arena>) {
        // SAFETY: `arena` is a live node of the list.
        let node = unsafe { arena.as_ref() };
        debug_assert!(node.next.get().is_none(), "a fresh arena has no link");
        node.next.set(self.arenas.get());
        self.arenas.set(Some(arena));
    }

    /// Unlink `arena` from the list of all allocated arenas.
    ///
    /// Walks to the predecessor instead of keeping a `prev` link on every node:
    /// this only runs when an arena is freed, which is at most once per
    /// [`SLOTS_PER_ARENA`] slot releases, and never on the allocation path.
    fn remove_arena(&self, arena: NonNull<Arena>) {
        // SAFETY: `arena` is a live node of the list.
        let node = unsafe { arena.as_ref() };
        let mut current = self.arenas.get();
        if current == Some(arena) {
            self.arenas.set(node.next.take());
            return;
        }
        while let Some(link) = current {
            // SAFETY: list nodes are live.
            let link = unsafe { link.as_ref() };
            if link.next.get() == Some(arena) {
                link.next.set(node.next.take());
                return;
            }
            current = link.next.get();
        }
        unreachable!("arena is not in the list of allocated arenas");
    }

    /// Put `arena` on the free list. A no-op when it is already there.
    fn push_free(&self, arena: NonNull<Arena>) {
        // SAFETY: `arena` is a live node of the list.
        let node = unsafe { arena.as_ref() };
        debug_assert!(
            node.free.get() != 0,
            "an arena without a free slot is not on the free list"
        );
        debug_assert_ne!(
            self.spare.get(),
            Some(arena),
            "the spare is not on the free list"
        );
        if node.on_free_list.replace(true) {
            return;
        }
        node.free_next.set(self.free_list.get());
        self.free_list.set(Some(arena));
    }

    /// Take the head of the free list.
    fn pop_free(&self) -> Option<NonNull<Arena>> {
        let head = self.free_list.get()?;
        // SAFETY: list nodes are live.
        let node = unsafe { head.as_ref() };
        debug_assert!(node.on_free_list.get(), "free-list node without the flag");
        node.on_free_list.set(false);
        self.free_list.set(node.free_next.take());
        Some(head)
    }

    /// Unlink `arena` from the free list, if it is on it.
    ///
    /// Same trade as [`Self::remove_arena`]: only an arena that empties leaves the
    /// list by anything but the head, so the walk stays off the allocation path.
    fn remove_free(&self, arena: NonNull<Arena>) {
        // SAFETY: `arena` is a live node of the list.
        let node = unsafe { arena.as_ref() };
        if !node.on_free_list.get() {
            return;
        }
        node.on_free_list.set(false);
        let mut current = self.free_list.get();
        if current == Some(arena) {
            self.free_list.set(node.free_next.take());
            return;
        }
        while let Some(link) = current {
            // SAFETY: list nodes are live.
            let link = unsafe { link.as_ref() };
            if link.free_next.get() == Some(arena) {
                link.free_next.set(node.free_next.take());
                return;
            }
            current = link.free_next.get();
        }
        unreachable!("arena is not in the free list");
    }

    /// Number of allocated slots across every spool.
    #[must_use]
    pub const fn live_slots(&self) -> usize {
        self.live.get()
    }

    /// Number of allocated arenas, spare included.
    #[must_use]
    pub const fn arena_count(&self) -> usize {
        self.count.get()
    }

    /// Debug-only consistency check: the two lists, the free bitmaps, the per-arena
    /// `used` counters and the global counters must all agree with the per-slot
    /// refcounts.
    #[cfg(debug_assertions)]
    pub fn assert_invariants(&self) {
        let mut live = 0;
        let mut arenas = 0;
        let mut current = self.arenas.get();

        while let Some(ptr) = current {
            arenas += 1;
            assert!(arenas <= self.count.get(), "arena list loop");
            // SAFETY: list nodes are live.
            let arena = unsafe { ptr.as_ref() };

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

            let is_spare = self.spare.get() == Some(ptr);
            assert_eq!(
                arena.on_free_list.get(),
                arena.free.get() != 0 && !is_spare,
                "free list membership out of sync"
            );
            if is_spare {
                assert_eq!(arena.used.get(), 0, "the spare holds live slots");
            }

            current = arena.next.get();
        }

        assert_eq!(arenas, self.count.get(), "arena count out of sync");
        assert_eq!(live, self.live.get(), "global live counter out of sync");

        let mut free = 0;
        let mut current = self.free_list.get();
        while let Some(ptr) = current {
            free += 1;
            assert!(free <= arenas, "free list loop");
            // SAFETY: list nodes are live.
            let arena = unsafe { ptr.as_ref() };
            assert!(arena.on_free_list.get(), "free-list node without the flag");
            current = arena.free_next.get();
        }
    }
}

impl Default for ArenaSet {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ArenaSet {
    fn drop(&mut self) {
        let mut current = self.arenas.get();
        while let Some(arena) = current {
            // SAFETY: `arena` is a live node of the list.
            let next = unsafe { arena.as_ref() }.next.get();
            // SAFETY: the node came from `Box::leak` in `grow` and is still linked,
            // and `&mut self` means no `SlotMut`/`SlotRef`/`SlotView` (each of which
            // holds an `Rc<ArenaSet>`) is alive to refer to it.
            unsafe { drop(Box::from_raw(arena.as_ptr())) };
            current = next;
        }
    }
}

/// Exclusive write access to a slot, owned by the spool writer.
///
/// Obtained from [`ArenaSet::alloc`] and holds that allocation's single reference
/// until dropped.
pub struct SlotMut {
    set: Rc<ArenaSet>,
    id: SlotId,
}

impl SlotMut {
    /// Adopt the reference returned by [`ArenaSet::alloc`].
    #[must_use]
    pub fn adopt(set: &Rc<ArenaSet>, id: SlotId) -> Self {
        Self {
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
        self.id.published()
    }

    /// Unwritten remainder of this slot.
    ///
    /// This is the only writable view of the slot, so it can never alias a
    /// reader's published range.
    pub fn tail(&mut self) -> &mut [u8] {
        let published = self.id.published();
        let len = SLOT_SIZE - published;
        let arena = self.id.owner();
        // SAFETY: `SlotMut` is the slot's single writer reference, so no other
        // reference to `[published, SLOT_SIZE)` exists: readers only see
        // `[0, published)`. The lifetime is tied to `&mut self`, which keeps the
        // guard (and therefore the arena) alive.
        unsafe { std::slice::from_raw_parts_mut(arena.tail_ptr(self.id.index(), published), len) }
    }

    /// Publish `len` bytes of this slot, making `[0, len)` visible to readers.
    ///
    /// # Panics
    ///
    /// Panics when `len` decreases or exceeds [`SLOT_SIZE`].
    pub fn publish(&self, len: usize) {
        self.id.owner().publish(self.id.index(), len);
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
    id: SlotId,
    offset: usize,
}

impl SlotRef {
    /// Pin `id` and start a cursor at its beginning.
    #[must_use]
    pub fn new(set: &Rc<ArenaSet>, id: SlotId) -> Self {
        id.retain();
        Self {
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
        let published = self.id.published();
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
        let Some(next) = self.id.next() else {
            return false;
        };
        next.retain();
        let previous = self.id;
        self.id = next;
        self.offset = 0;
        self.set.release(previous);
        true
    }
}

impl Clone for SlotRef {
    /// A second cursor at the same position: same slot, same offset. It continues
    /// from where the source is; there is no rewind.
    fn clone(&self) -> Self {
        self.id.retain();
        Self {
            set: Rc::clone(&self.set),
            id: self.id,
            offset: self.offset,
        }
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
    id: SlotId,
    range: Range<usize>,
}

impl SlotView {
    #[must_use]
    pub fn new(set: &Rc<ArenaSet>, id: SlotId, range: Range<usize>) -> Self {
        id.retain();
        Self {
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
        let bytes = self.id.owner().bytes(self.id.index(), self.range.end);
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

    /// `assert_invariants` only exists with debug assertions; release builds
    /// (where the throughput benchmark runs) skip the check.
    fn check(set: &ArenaSet) {
        #[cfg(debug_assertions)]
        set.assert_invariants();
        #[cfg(not(debug_assertions))]
        let _ = set;
    }

    /// Allocate exactly `arenas` full arenas worth of slots, in allocation order.
    fn fill(set: &ArenaSet, arenas: usize) -> Vec<SlotId> {
        (0..arenas * SLOTS_PER_ARENA)
            .map(|_| set.alloc().expect("slot"))
            .collect()
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
        assert_eq!(id.refs(), 1);

        drop(pin);
        assert_eq!(set.live_slots(), 0);
        // The emptied arena is kept as the spare instead of being freed.
        assert_eq!(set.arena_count(), 1);

        // The bitmap hands the same slot back.
        assert_eq!(set.alloc(), Some(id));
        check(&set);
    }

    #[test]
    fn linked_slots_hold_a_reference() {
        let set = set();
        let a = set.alloc().expect("slot");
        let b = set.alloc().expect("slot");

        a.link(b);
        assert_eq!(b.refs(), 2);

        // Releasing the writer's reference to `a` cascades into `b`, which keeps
        // its own reference.
        set.release(a);
        assert_eq!(set.live_slots(), 1);
        assert_eq!(b.refs(), 1);

        set.release(b);
        assert_eq!(set.live_slots(), 0);
        check(&set);
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
        a.link(b);
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
        check(&set);
    }

    #[test]
    fn a_cloned_cursor_continues_at_the_same_offset() {
        let set = set();
        let a = set.alloc().expect("slot");
        let b = set.alloc().expect("slot");

        let mut writer = SlotMut::adopt(&set, a);
        let mut reader = SlotRef::new(&set, a);
        writer.tail()[..4].copy_from_slice(b"abcd");
        writer.publish(4);

        assert_eq!(reader.take_range(), Some(0..4));
        let mut clone = reader.clone();
        assert_eq!(clone.offset(), 4);
        assert!(clone.take_range().is_none(), "a clone does not rewind");

        writer.tail()[..2].copy_from_slice(b"ef");
        writer.publish(6);
        assert_eq!(clone.take_range(), Some(4..6));
        assert_eq!(reader.take_range(), Some(4..6));
        assert_eq!(a.refs(), 3, "writer, reader and clone each pin the slot");

        drop(clone);
        drop(reader);
        drop(writer);
        set.release(b);

        assert_eq!(set.live_slots(), 0);
        check(&set);
    }

    #[test]
    fn arenas_grow_on_demand() {
        let set = set();
        assert_eq!(set.arena_count(), 0);

        let ids = fill(&set, 2);
        assert_eq!(set.arena_count(), 2);
        assert_eq!(set.live_slots(), 2 * SLOTS_PER_ARENA);
        assert_ne!(ids[0].arena, ids[SLOTS_PER_ARENA].arena);
        check(&set);

        for id in ids {
            set.release(id);
        }
        // One empty arena survives as the spare.
        assert_eq!(set.arena_count(), 1);
        assert_eq!(set.live_slots(), 0);
        check(&set);
    }

    #[test]
    fn the_boundary_reuses_the_spare_instead_of_reallocating() {
        let set = set();
        let ids = fill(&set, 1);
        let arena = ids[0].arena;
        for id in ids {
            set.release(id);
        }
        assert_eq!(set.arena_count(), 1);

        // Allocate and release right at the arena boundary: neither the arena nor
        // the count may move, or every such slot costs a zeroed 6 MB allocation.
        for _ in 0..8 {
            let id = set.alloc().expect("slot");
            assert_eq!(id.arena, arena);
            assert_eq!(id.index(), 0);
            assert_eq!(set.arena_count(), 1);
            check(&set);

            set.release(id);
            assert_eq!(set.arena_count(), 1);
            check(&set);
        }
    }

    #[test]
    fn a_middle_arena_can_be_dropped() {
        let set = set();
        let ids = fill(&set, 3);
        let (first, middle, last) = (
            ids[0].arena,
            ids[SLOTS_PER_ARENA].arena,
            ids[2 * SLOTS_PER_ARENA].arena,
        );
        assert_ne!(first, middle);
        assert_ne!(middle, last);

        // One live slot in the first and last arena, and the middle emptied: it
        // becomes the spare, so nothing is freed yet.
        for id in &ids[1..SLOTS_PER_ARENA] {
            set.release(*id);
        }
        for id in &ids[SLOTS_PER_ARENA..2 * SLOTS_PER_ARENA] {
            set.release(*id);
        }
        for id in &ids[2 * SLOTS_PER_ARENA + 1..] {
            set.release(*id);
        }
        assert_eq!(set.arena_count(), 3);
        assert_eq!(set.live_slots(), 2);
        check(&set);

        // Emptying the last arena retires the spare, which frees the middle node.
        set.release(ids[2 * SLOTS_PER_ARENA]);
        assert_eq!(set.arena_count(), 2);
        assert_eq!(set.live_slots(), 1);
        check(&set);

        // The surviving arenas still allocate and release: the first arena is
        // free slots, so it is preferred over the spare.
        let id = set.alloc().expect("slot");
        assert_eq!(id.arena, first);
        assert_eq!(set.arena_count(), 2);
        set.release(id);
        set.release(ids[0]);

        assert_eq!(set.live_slots(), 0);
        assert_eq!(set.arena_count(), 1);
        check(&set);
    }

    #[test]
    fn an_arena_with_free_slots_is_filled_before_growing() {
        let set = set();
        let ids = fill(&set, 2);
        // Free all but one slot of the first arena; the second stays full.
        for id in &ids[1..SLOTS_PER_ARENA] {
            set.release(*id);
        }
        assert_eq!(set.arena_count(), 2);

        let more: Vec<SlotId> = (0..SLOTS_PER_ARENA - 1)
            .map(|_| set.alloc().expect("slot"))
            .collect();
        assert_eq!(
            set.arena_count(),
            2,
            "no growth while an arena has free slots"
        );
        assert!(more.iter().all(|id| id.arena == ids[0].arena));
        check(&set);

        for id in more {
            set.release(id);
        }
        set.release(ids[0]);
        for id in &ids[SLOTS_PER_ARENA..] {
            set.release(*id);
        }
        assert_eq!(set.live_slots(), 0);
        assert_eq!(set.arena_count(), 1);
        check(&set);
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
        id.retain();
        let _ = keep;
    }
}
