# Slot arena rework

Status: implemented in `crates/twitch/src/{arena,spool,constants}.rs`. This note
keeps the reasoning, which is not obvious from the code.

## Problem

The old arena was a fixed `[Option<Rc<Arena>>; MAX_ARENAS]`:

- `alloc()` scanned the array from index 0 on every slot.
- The array was the id space (`SlotId { arena: u16, index: u8 }`), so the slot
  table had to be sized from `MAX_ARENAS` (`MAX_SLOTS` spool records).
- A "hard ceiling across all broadcasts" (`MAX_ARENAS = 6 * 4`) was really a
  guess that had to scale with the number of streams a daemon serves.
- An emptied arena was dropped immediately, so a pool hovering at an arena
  boundary paid a zeroed 6 MB `alloc_zeroed` (and a free) per slot.
- `Arena::new` called `handle_alloc_error`; with no ceiling that turns "a
  consumer stalled for 30 s" into an aborted process instead of one failed
  segment.

## Decisions

**Identity is the arena address.** `SlotId { arena: NonNull<Arena>, index: u8 }`,
`Copy`, 16 B. No id→arena table and no id walk; a live id always points at a live
arena because an arena is only freed when its last slot is released, and the id's
slot holds a reference. `Debug` prints the address plus the index (so tests must
not assert on id ordering). The slot index stays slot-relative.

**Arenas are intrusive list nodes.** One singly linked list of every allocated
arena (middle nodes can be dropped, `Drop for ArenaSet` walks it), plus a second
singly linked list — the *free* list — of exactly the arenas with at least one
free slot. Neither needs a `prev` link: the allocation path only ever takes the
head, and the only other removal is the arena that just emptied (once per 32 slot
releases), so splicing walks to the predecessor instead of paying for a back-link
on every node. `alloc()` unlinks the head of the free list (O(1), no scanning) and
puts it back if it still has room; a release that frees a slot in a full arena
pushes it back. The bitmap is still the source of truth for which slot is free.

**One spare.** An arena that empties is unlinked from the free list and kept
as the single spare; the next allocation the free list cannot serve consumes
it. If a spare already existed, the older one is freed. The spare counts in
`arena_count()`, so a test that releases everything asserts 1, not 0. This is
what removes the boundary churn: allocate/release at a full arena never moves
`arena_count()`.

**No global cap.** `Arena::new` returns `Option`, and a failure surfaces as
`ArenaFull` — the same error the per-spool cap produces — so the downloader backs
off (40 × 50 ms) and then drops that one segment. A ceiling would just be death
at an arbitrary number instead of death at the real one.

**The spool record merged into the writer block.** What the old `SpoolSlot`
table held was only: `refs` (to know when to free the record), `closed`, and
`published` (the total, used as the "something changed" token). All three belong
to the writer block:

```rust
struct Spool { arena: Rc<ArenaSet>, published: Cell<usize>, closed: Cell<bool> }
pub struct Writer { spool: Rc<Spool>, wakes: Rc<WakeSet>, tail: SlotMut, slots: usize }
pub struct Reader { spool: Rc<Spool>, cursor: SlotRef }
```

The writer plus every reader holds one `Rc`; the last of them frees the block.
`SpoolId`, `SpoolSlot`, `free_spools`, `MAX_SLOTS` and the `*_spool` methods are
gone. The wait predicate did not change: `published != seen || closed`.

**`Reader::Clone` copies the cursor** (same slot, same offset) instead of
re-pinning a head. There is no `rewind()` and no `Reader.head`: a cursor cannot
go backwards, and the scheduler's window/showing readers are the only clone
sources and never advance. A clone that starts mid-segment is legal and means
"continue from here".

**Cap per segment.** `SEGMENT_SLOTS_MAX = 64` (12.3 MB) replaces `SLOT_BUDGET`,
which was derived from the arena sizing that no longer exists. Twitch caps a
source at 8 Mbps, so a 10 s DVR segment is ~10.6 MB. This is now the only way a
spool write fails.  `Spool::open_with_cap` lets the disk output give DVR chunks
their own cap (`DVR_SLOTS_MAX = 96`, ~18.5 MB) and their own `ArenaSet`, so a
12 Mbps 10 s DVR chunk (~15 MB) neither overflows the live cap nor competes with
live spools for slots.

**`SlotView` derives its base from `SlotId`** instead of holding a second
`Rc<Arena>`; the pinned slot already keeps the arena alive.

## Spool and wakeups

- Slots are `192_512` B (`188 × 1024`, page-aligned), 32 per arena, `alloc_zeroed`
  so the first touch maps whole pages; every view derives from one base pointer
  (Miri-checked under Stacked Borrows).
- A `SlotView` is an `IoBuf`: readers hand arena memory straight to
  `write_all`/`write_at`, so the fan-out is zero-copy.
- A spool has no wait list. Each consumer task owns one `ConsumerWake` (one
  `Waker`, allocated once) registered in the scheduler's `WakeSet`; a publish
  wakes every registered consumer and each re-checks its own spool frontier.
  Waking on close only was the old design, and it turned live streaming into one
  burst per segment. See `spool.rs::Wait`.

## Deleted

`SpoolId`, `SpoolSlot`, `free_spools`, `alloc_spool`/`retain_spool`/
`release_spool`/`publish_spool`/`close_spool`, `spool_published`/`spool_closed`,
`MAX_SLOTS`, `MAX_ARENAS`, `MAX_BROADCASTS`, `ARENAS_PER_BROADCAST`,
`SLOT_BUDGET`, `SlotId::arena() -> u16`, `Reader::rewind`, `Reader.head`.

## Tests

`arena.rs`: growth on demand, an arena with free slots filled before growing, the spare
reused at the boundary (`arena_count()` stable), a middle arena dropped, cloned
cursor continuation, plus the existing refcount/chain tests. `spool.rs`: the
64-slot cap, a clone starting at the cursor, EOF via `closed`. All run under
Miri, which is also what catches a double free in the list splices.
