# DXGI presentation: what the seam costs, and what it cost us

Session: `ses_e08cb9d0fffdpZRKhlX2DhK71f` — the transcript behind this document,
including the measurements, the dead ends and the two crashes. Read it before
re-running any of the experiments below; it records which ones were already shown
to be worthless.

A record of one session: fourteen hours of wall clock, and — by the measure that
matters to whoever reads this next, which is how much context it consumed and how
many dead ends it opened before closing — about a week's worth of work. Both
numbers earn their place. The second says what re-entering this will cost; the
first says it did not take a fortnight of calendar, and most of it went on
theories held without measurement rather than on the work itself.

The working version of the conclusion lives on the jj bookmark **`blit-path`**
(`244bd6aa`); the tree it was developed against is `f2c68bfe`, which is now part
of `main`.

## The shape of the problem

libplacebo is Vulkan. Windows presents through the DXGI flip model, and that is
the only first-class presentation path on the platform. Vulkan WSI does not
avoid DXGI — the ICD layers on it. NVIDIA even exposes the choice
(*Vulkan/OpenGL Present Method*: `Auto`, `Prefer native`, `Prefer layered on
DXGI Swapchain`): "layered" is DXGI underneath, "native" is the vendor's own
flip path. Either way the seam is inside the driver, where an application can
neither see it nor fix it, and the failure mode reported from that seam is
deadlock. That is why this project owns the DXGI swapchain.

So the architecture is fixed by that decision, and only one question is left:
**how do Vulkan's pixels get into a DXGI backbuffer?**

## Two ways, and only one of them works

| | share | blit |
|---|---|---|
| render target | the imported DXGI backbuffer | a D3D12-owned canvas, one per backbuffer |
| who writes the backbuffer | Vulkan | D3D12, as a copy |
| what the D3D12 queue carries | `Wait`s only | a real command list, every frame |
| what DXGI has to order the scanout against | nothing | that command list |
| cost | none | one full-canvas copy per frame |

The share path looks free and is not. D3D12 never touches the backbuffer, so the
queue's submission fence — the thing DXGI's flip-queue accounting is built
around — never advances with our work. `Present` is issued, the compositor's
read is not gated, and the display samples the buffer while libplacebo (or, later,
our own pass) is still drawing into it. That is the band.

The blit path is `vk_dxgi`'s `DVK_SWAPCHAIN_BACKBUFFER_BLIT_FLAG` mode, and it is
the one the library calls more compatible. The backbuffer is only ever written by
D3D12, in `COPY_DEST`, and barriered to `PRESENT` before the present. There is
real queue work, so the ordering holds.

## What the measurements said

- **The finished buffer was always clean.** A compute pass scanned each target
  for the fill colour and wrote its tally into a persistent host-visible buffer
  (read by the CPU with no fence, no copy). With every synchronisation edge
  disabled it reported zero bands. The content was right; the display was reading
  it early.
- **The interface texture is opaque.** 0 of 760 000 texels translucent, read back
  from the app. The fill only ever showed through geometry, and in a finished
  frame there was none.
- **Band height is the race, quantified.** Bands got thinner when libplacebo left
  the path: one pass instead of several means less of the frame is undrawn when
  the scanout samples it.
- **The fence itself was never wrong.** The D3D12 fence imported as a Vulkan
  timeline carries values correctly — `GetCompletedValue` tracked the submitted
  value. The failure was what the *queue* did with it.
- **An empty command list per frame was not enough.** Real queue work is what
  gates the scanout; a submission that touches nothing does not.

## Experiments already run (do not repeat)

Each of these was carried out and answered; the answer is in the transcript, the
summary is here so a future session does not re-derive it by hand.

| experiment | what changed | result | conclusion |
|---|---|---|---|
| `SIPHON_PLAIN` | ZGUI's texture blitted straight into the DXGI image, no overlay pass at all | bands survived, and it TDR'd | libplacebo's overlay pass is not the cause |
| `SIPHON_DEBUG_FILL` | magenta underlay instead of the pass clear | black bands became magenta bands | the hole is real, and the fill is the detector |
| `SIPHON_UI_CONST` / `TWICE` / `NONE` / `OPAQUE` / `SIPHON_BLIT` | overlay-pass variants: flat colour, doubled, underlay only, alpha ignored, plain blit | flat-colour frames were uniform in every capture | the defect does not live in the overlay parts |
| `SIPHON_CATCH_BANDS` | download the target after the render, before the present, scan for the fill | never fired | reading back stalls the frame and hides the race it is meant to catch |
| GDI `CopyFromScreen`, idle, under mouse motion, 40 programmatic resizes, 30 real mouse-driven resize frames | none | never showed magenta, not once | a 3 ms tear cannot be captured at 10–30 Hz; stop treating capture as the oracle |
| full-frame, then per-column, interface alpha readback | none | 0 of 760 000 texels translucent | the fill does not bleed through the interface |
| `D3D12` `COMMON → PRESENT` after `ResizeBuffers` | transition every backbuffer before its first present | `E_INVALIDARG` at `Close` | backbuffers are already `PRESENT`; there is no state to fix |
| frame-latency waitable bypassed (`wait_for_frame` → `true`) | presentation paced only by `Present` | the counter rose to ~380/s | **withdrawn**: the counter counts *attempts*, not presents, so it proved nothing. The 10 fps was concurrent with `ResizeBuffers` failing (see the leak below); the gate is fine |
| waitable re-taken after `ResizeBuffers` | close the old handle, `GetFrameLatencyWaitableObject` again, re-apply the latency | no change | nothing to fix; the handle was never the problem |
| `SIPHON_NO_SIGNAL` | queue `Wait` on a value that is never signalled | 10 fps, presents stopped retiring | the D3D12 queue wait is genuinely honoured |
| `SIPHON_SYNC=none / compose / handover / present / all` | CPU waits on each of the three edges, in every combination | no combination removed the bands on the share path | the share path has no ordering hook to strengthen |
| empty `ExecuteCommandLists` per frame | a real submission that touches nothing | no change | DXGI needs work that *writes* the buffer, not merely a submission |
| one canvas / two canvases / one per backbuffer | rotate the render target | one canvas: ~5 s then a wedge (write-after-read); two: workable; three: the wait is three frames old and costs nothing | one canvas per backbuffer |
| Vulkan wait on the previous copy removed | render without waiting for the copy that read the canvas | 320 fps for ~5 s, then a permanent stall | with a single canvas this is corruption, not a speed-up |
| `drain()` at the top of `resize` | block until the D3D12 queue is idle | frozen UI, 4 fps | never block the UI thread on a queue in a resize path |
| `pl_tex_download` on the interface texture while libplacebo held it | none | libplacebo assertion `!tex_vk->held` | a held texture may only be read after `release` |

Earlier in the same investigation, and already reverted: a canvas-growth guard, an
in-move-loop device sync, and a drag frame-rate cap. All three either broke
painting or made the drag worse.

## Four bugs that were independent of the choice

1. **A failing `ResizeBuffers` is indistinguishable from a dead waitable, and
   I misread it as one.** The 10 fps after the first resize was taken for the
   frame-latency waitable going silent, and "fixed" by bypassing the gate. The
   real cause was the leaked references below breaking `ResizeBuffers`, which
   leaves the swapchain in a state where presents stop retiring — the same
   symptom. Measured since, on the tree without any D3D12 copy: a resize costs
   nothing, `160–165` presents per 500 ms either side of it. The gate stays.
2. **`ManuallyDrop` resource references leak.** `D3D12_TEXTURE_COPY_LOCATION
   .pResource` and `D3D12_RESOURCE_TRANSITION_BARRIER.pResource` are
   `ManuallyDrop<Option<ID3D12Resource>>`: the clone each one needs is not
   released by dropping the structure. One leak per frame per structure leaves
   DXGI with outstanding references, `ResizeBuffers` starts returning
   `DXGI_ERROR_INVALID_CALL`, and the chain can never grow again — the window
   then shows the swapchain background where the chain no longer covers it.
   Every `ManuallyDrop<Option<IUnknown>>` in a D3D12 structure needs an explicit
   drop after the call.
3. **Blocking waits in the resize path.** `drain()` plus `vkDeviceWaitIdle` per
   rebuild, and once per resize event, is 4 fps and a frozen window. One wait is
   enough: the last copy was queued behind the render that fed it, so a queue
   drain implies the Vulkan side is idle too.
4. **Backbuffers are already in `PRESENT`.** `GetBuffer` hands them over in
   `D3D12_RESOURCE_STATE_PRESENT`; a `COMMON → PRESENT` transition is invalid and
   fails with `E_INVALIDARG` at `Close`. There is no state to fix — nobody had
   told D3D12 otherwise.

## The DPC watchdog

The machine took a `DPC_WATCHDOG_VIOLATION` twice, both on the blit branch, both
while an automated resize storm (40 `SetWindowPos` at 25 ms) ran against a
present loop with its pacer removed (~380 presents/s on a 320 Hz panel). A DPC
watchdog is the driver's kernel component timing out; a user-mode app reaches it
by driving a driver path far harder than any user would. The pattern to avoid is
specific and worth writing down:

- never remove a pacer to test a hypothesis — replace it with a different limiter
  first;
- never pair a storm script with an unthrottled present loop;
- one variable per run.

The current tree does none of this: no shared canvases, no cross-API timeline, no
scan pass, and the stock `wait_for_frame`. The risk lives entirely on the
bookmark.

## Two more things worth keeping

- **The fill is the detector, and it belongs on the far side of the seam from the
  thing under test.** Drawing it inside wgpu would collapse two suspects into one
  observable. It is a Vulkan clear, under a quad that samples the wgpu texture.
- **`VK_KHR_external_semaphore_win32` is not enabled by wgpu.** Importing the
  D3D12 fence as a Vulkan timeline needs it, so the bookmark carries a vendored
  `wgpu-hal` 30.0.2 with one added line in `src/vulkan/adapter.rs` and a
  `[patch.crates-io]`. That is the only reason the vendor copy exists; if the
  fence goes, so does the patch.

## Where it stands

`blit-path` renders through the blit mode: Vulkan into per-backbuffer canvases,
D3D12 copy and `PRESENT` barrier, present behind real queue work. It showed no
bands, no TDRs, and stable throughput across resizes.

Remaining before it is shippable:

1. One sync instead of two in `rebuild` (the instrumentation is in the commit).
2. Pool the canvases by size, so the shrink at drag end donates to the next grow
   and only the first drag on a monitor pays full price (~200 ms today).
3. Strip the diagnostics (`SIPHON_SYNC`, the fence/rebuild prints, the scan is
   worth keeping behind a flag).
4. Confirm the DPC does not survive pooling and the sync reduction. If it does,
   stop rather than push: that would be the driver, and this path is not worth
   the machine.

What not to revisit: the share mode. It has no ordering hook for DXGI, and no
amount of CPU waiting fixes a backbuffer being recycled underneath the renderer.

## Rules that came out of it

- Own the swapchain; never hand presentation to the driver's layering.
- A D3D12 queue that executes no command lists has no ordering to offer DXGI.
- Present a backbuffer only after D3D12 has written it in the same queue.
- One canvas per backbuffer, so the wait on a canvas is three frames old and
  costs nothing; waiting on last frame's copy is a tenth of a second of
  cross-API latency.
- Synchronise at the smallest boundary, and delete the boundary if you can.
- Measure before theorising. A GPU-side counter that costs no sync is worth ten
  screen captures that cannot see a 3 ms tear.
