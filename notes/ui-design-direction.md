# UI design direction

Status: first pass, design only. No implementation yet. HDR is deliberately
excluded from this pass.

The current layout is broadly right; the problem is the *surface*: it reads as
a generic dark SaaS template. The target is a dense desktop tool with 90s DNA:
square where it can be, textured where it helps, colourful where colour carries
meaning, and functional first. Not pure brutalism, and not a web dashboard.

## System approach

- Do **not** invent a design system from scratch. Keep ZGUI's existing token
  system (`zgui-ui-tokens`, `ThemeProvider`, `Theme::with_css`) and treat it as
  the base to tweak.
- Start from the built-in `Terminal` preset as the closest baseline: radius 0,
  no motion, accent-driven. Override its palette and any remaining defaults in
  the app's `THEME_CSS` / `SHEET` rather than replacing the schema.
- Set `--zui-radius-base: 0` globally. The one approved exception is the **top
  rounding of the tab buttons** (`10px 10px 0 0` today) — keep it.
- Switches are not exempt from the square rule: the stock pill is overridden
  with a hard-edged track and square thumb, and sized up (40×22 default,
  34×18 for the dense per-channel rows), because the 24×14 pill was both the
  wrong shape and too small. Use the `--zui-color-control-switch-*` tokens for
  the track/thumb so the palette stays in one place.
- The checked colour is per channel: `--channel-color` is set from the row's
  Twitch `chatColor`. Twitch leaves that null until the user picks one, so the
  daemon walks the channel id by the golden angle into a stable `oklch(L C H)`
  colour: any hue, L 0.70–0.73, C 0.13–0.15. Global settings fall back to the
  app primary. The expand chevron uses the same channel colour.
- The app accent is a cool blue (`oklch(0.75 0.13 230)`), not the earlier amber —
  amber read too close to Pornhub. Live red and error red stay semantic.
- Every neutral surface and text colour is written in OKLCH at the same hue
  (240): the empty table, odd/even rows, hover, the details panel and borders
  differ only in lightness and chroma, so they cannot drift into different
  hues. Semantic reds and per-channel colours are the only other hues. Each
  value repeated in `SHEET` is declared once in its `:root` palette and used
  through `var()`; the widget palette lives in `THEME_CSS`.
- The channels scrollbar lives in a permanent right strip (`--zui-scrollbar-width`
  is 8px; the engine reserves about 16px of gutter for its bar, and
  `scrollbar-gutter` is unsupported in ZGUI). The rows and the panel paint
  *under* it with a `box-shadow` strip in their own colour (16px, no blur),
  which is ink and leaves the layout alone; a negative margin would widen the
  box into the gutter instead and fight the viewport's horizontal clip. The
  cells keep 16px of right padding so they sit clear of the bar, and the header
  keeps 32px so it ends where the cells do.
- The list only scrolls while `.tabbody` has `min-height: 0`. Without it the
  tab body takes its content height, the `ScrollArea` never overflows, the add
  bar is pushed below the window, and the list looks stuck after one row of
  wheel travel. The body has to shrink so the filter bar stays anchored and the
  viewport gets the remaining height.
- The engine paints its own scrollbar with the scrollport, *before* the rows,
  so the gutter strips cover its thumb. `ListThumb` draws the visible thumb
  after the rows from the same `ScrollAreaContext` position signal, and takes
  no pointer events: drags and track paging still reach the engine's bar
  underneath. The engine's own thumb is themed transparent so it cannot show
  through the empty area below the last row.
- Motion is back for these controls: the switch track/thumb transition over
  160ms, a live dot pulse, the two marquees, the tab weight, the row chevron's
  90-degree turn over 160ms, and the expanded panel's 200ms roll. Tab names use
  Chillax, whose `wght` axis tops out at 700: unselected tabs sit at its
  minimum 200 and the selected tab at 600, with a cubic ease-out over 250ms
  while the outgoing tab eases back; the outgoing tab is tracked explicitly so
  the other tabs stay at 200 instead of blinking through the ramp. Each label
  keeps a fixed-width box, measured once for Chillax at the selected weight, so
  the tween cannot resize the tab or shift the row; the text is centred inside
  that box, which keeps it centred when the font falls back to the system's and
  the glyphs spill symmetrically rather than hanging off one side. The string
  must be an explicit child of that box: a `text` element styled as a flex
  container turns its own string into an anonymous flex item, and this zgui
  revision mis-shapes that item once the variation changes, painting the
  selected label into another tab and dropping others. Driven by
  `font-variation-settings` and a per-frame signal.
  The switch gets its
  motion from `--zui-motion-duration-normal`: zgui-ui's switch declares its own
  `transition-property` and reads that token, and the Terminal preset sets it
  to `0ms`, so the token is overridden in `THEME_CSS` rather than redeclaring
  transitions in the app sheet. The live dot is a drawn box, not a glyph: at
  the pinned zgui revision a translated glyph animates and paints fine, but a
  `scale()` transform drops the glyph except at its rest keyframe (verified on
  the live Wayland window with kwin-mcp). Text motion is translation only;
  the tab weight moves a font axis, not a transform. The thumb keeps a 2px
  inset at both ends of its travel. Hover
  tooltips wait 400ms so dragging the pointer across a row does not flash a
  label.
- Avoid the default "sloth look": no purple `#9146ff` primary, no soft cards on
  empty space, no oversized radii, no random low-contrast grey text.
- Use **semantic colour**, not one accent. Live/record/healthy/error/selection
  should be distinct and meaningful (red live dot, amber record, etc.). OKLCH
  is supported by ZGUI, so palette work should be done in L/C/H values, not
  guessed hex.
- Texture and patterns are wanted, but only on large surfaces. The first
  scanline pass on the channel list was rejected: the empty area under the rows
  must stay solid dark. Exact pattern/placement is still open.

## Density and spacing

- Denser than today: less padding everywhere, smaller row height, tighter
  groups. Use a 4px spacing grid and explicit control-height tokens:
  - 28px (1.75rem) for compact rows / small controls
  - 32px (2rem) for buttons and inputs
  - 40px (2.5rem) for tabs
- 1px hairline borders, absolute pixels; borders do not scale with type.
- Root `font-size` stays 16px. UI body text 14px (0.875rem); metadata 12px
  (0.75rem); headings 20px/24px. Use `rem` for rhythm, px for structure.

## Typography

- **Archivo** variable font for all UI text. The TTF ships in
  `crates/ui/fonts/` (OFL 1.1); axes `wght` 100–900 and `wdth` 62–125. ZGUI
  supports variable fonts through `font-variation-settings`; `font-weight`
  alone does not drive the axes. The narrow `wdth` axis is useful for dense
  table rows.
- **Chillax** (Fontshare, ITF Free Font License 2.0) is the brand face behind
  the `BrandFont` family. `crates/ui/build.rs` downloads the unmodified
  variable TTF from `https://api.fontshare.com/v2/fonts/download/chillax` and
  writes it into the profile's `OUT_DIR`, so both profiles embed it and a
  `cargo clean` makes the next build fetch it again; `SIPHON_CHILLAX_TTF`
  points the build at a local copy instead, and `SIPHON_FONT_BRAND` still
  overrides the family at runtime. The licence allows embedding in the app but
  explicitly forbids distributing the font file through a repository, so it is
  **not** in git.
- **Noto Color Emoji** (CBDT build, OFL 1.1) ships in `crates/ui/fonts/` and is
  registered as `SiphonEmoji`. It is both the last entry in the root
  `font-family` stack and bound to fontique's `Emoji` generic. The CBDT build
  matters: the system COLRv1 font decodes as a monochrome outline in zgui's
  swash 0.2.10. Verified with kwin-mcp that 😀/🎉/✅ render in colour. `★` and
  `✧` are not Unicode-emoji, so they stay monochrome by design.

## Header / activity ticker

- The compositor's title bar already carries "Siphon", so the in-app wordmark
  was redundant. Its row is now a news ticker over the channel stream titles
  (`name — title`) followed by the newest notifications (`summary — body`),
  separated by `│`. Titles belong in the top marquee, not in the table rows.
- The run repeats eight times and slides by `-12.5%` (one copy) on a linear
  loop whose duration scales with the text length. Eight copies keep the window
  full even for one short title and no notifications. Connection health lives at
  the right end of the **tab row**, not in the ticker; the update banner still
  sits at the right of the ticker row when present.
- Keep the 14px top padding and the 6px gap below the 30px strip. Deleting the
  space entirely makes the tabs look clipped against the title bar; filling
  it with live activity is what makes the gap read as deliberate. The ticker
  viewport is a flex row with `align-items: center` and full header height, so
  the run stays centred both windowed and maximized.
- Do not use `scale()` on the ticker text (see the transform note above).

## Channels view

- The channels table must **fill the entire width** of the content area. It
  currently reads as a small list floating on an empty background.
- Remove the visual gap between the tab bar and the channels content. Add a
  full-width divider/line spanning the whole window between them.
- Alternate row colours (zebra striping) so the dense list remains scannable.
  The neutral ramp is a three-step one in OKLCH at hue 240: empty table
  `0.11`, odd rows `0.145`, even rows `0.165`, hover `0.19`, open `0.215`.
  The chrome/root background is `0.16`. The dark rows need their own explicit
  colour — if they are left transparent they inherit the table background and
  the banding disappears below the last row. Offline rows step their name and
  title back (`0.78` / `0.56`) so the live ones carry the eye.
- The header is one row of identical type (10px, 600, 0.6 letter-spacing) and
  each label sits over its column: the notify switches are labelled once, with
  `Notify` centred over `live` and `title`, instead of repeating the two words
  on every row. The header and the rows both keep 32px of right padding (the
  engine's 16px scroll gutter plus the cells' own 10px, plus the old 6px list
  gap folded in) so the last columns sit over their values while the row
  background runs past them; `Viewers` is right-aligned like the counts.
- Rows are 36px tall: the 24px avatar plus 6px top and bottom padding. The
  two switches no longer set the row height, so the padding is explicit. Each
  row carries a transparent 2px left edge that an open row paints in its
  channel colour; the cells line up with the header because the edge is
  reserved on both.
- Order is meaning: pinned rows first, then live, then the channel that was
  most recently live, with ties keeping config order. Live rows are never
  ranked by viewers — a list that reshuffles every counter tick cannot be
  reached for — and the sort only runs when a row's key changes (a live flip,
  a pin, a new `last live`). Pins are a client preference (`ui.json`), so two
  windows on one daemon may order differently.
- The `Uptime` column is the row's clock: live rows show how long the stream
  has been running (`2h 14m`), offline rows show `2h ago` in muted type,
  never-live rows `—`. The dot already says live/offline, and the viewer
  count belongs to the expanded detail. It is right-aligned, header included.
  A minute-granular label needs a clock: one shared `now` signal ticks every
  30 seconds and every relative label reads it.
- The row's other states lead the title, not a column of their own: a
  red-outlined `!` for any failure (the channel's own resolve or its
  pipeline/disk), an amber `REC` pill while a recording is writing, and a drawn
  pin for a pinned row. The title cell's tooltip is the failure message when
  there is one and the stream id otherwise.
- Tab badges carry what is worth seeing from another tab: the error count on
  `Channels` and the recording count on `Recordings`. The badge's slot is
  always reserved, so a count appearing cannot widen the tab row.
- Row actions stay out of the row: `Pin`/`Unpin` and `Remove` live in the
  expanded detail, and the same two are in a right-click context menu as an
  accelerator. The menu is never the only home — a row whose actions are only
  reachable by right-click is a row a keyboard cannot use. Removing raises a
  toast with `Undo` for eight seconds; the undo is a real re-add carrying the
  row's switches (`AddLogin { login, switches }`), and its pin. The stack is
  lifted clear of the bottom add bar rather than sitting over it.
- The switch override must select `.zui-switch--sm`: this zgui revision emits
  the class and no `data-size` attribute, so a `[data-size="sm"]` rule silently
  leaves every switch at 40x22.
- Empty and pending states are explicit: a list with no channels says how to
  add one, and a resolving login keeps every column aligned.
- Keep the row's avatar/display name and the live dot; those are the primary
  identity and state. Display names use Chillax (see Typography).
- Row titles are single-line and truncated to one cell height; the full text
  is also in the top marquee and the expanded detail panel. Titles over 55
  characters scroll horizontally **for as long as the row is hovered** and
  **rewind to the start** on pointer-leave. The run is three copies of
  `title + "     │     "`, so one copy is a third of its width: the offset is
  kept as a fraction of a copy and rendered as `translateX(-p * 33.333%)`,
  which wraps exactly on the copy boundary without measuring the text (emoji
  would break any per-character width estimate). The pace is characters per
  second, one copy per `copy_chars / 8.5` seconds, and every frame's step is
  scaled by that frame's own duration, so the marquee is frame-rate
  independent and a long title does not race. The offset is a signal driven
  per frame through zgui's `request_frame` (not compio's timer, which panics
  outside its runtime), so leaving mid-scroll can return smoothly; a
  generation counter cancels an older tween when the pointer moves again.
- Row hover/open/pending states should be subtle background changes, not
  rounded cards.

## Expanded channel detail

- Do not repeat the avatar and display name from the row. Show only the Twitch
  handle (`@login`) in the detail header.
- Show the full stream title here, wrapped; the row only shows a truncated copy
  to keep the Notify column aligned.
- Remove the duplicate LIVE badge from the detail header; the row already has
  the red live dot.
- No `createdAt` anywhere in the happy path. It was the same GQL
  `stream.createdAt` already parsed into `stream_start_ms`; keeping both was
  redundant. The UI shows only `Started`, and the daemon drops the raw string
  too — if Twitch ever sends a timestamp it cannot parse, that is an error-path
  warning with the raw value, not state carried through the system.
- Keep Channel ID, Stream ID, Game ID/Game, Viewers and Collab in the detail
  grid. The grid itself should be denser and flatter than today.
- The panel is part of the row, not a card under it: no top border, no margin,
  and the open row's channel-coloured 2px edge runs straight down through it.
  Its background is a step darker than the open row (`0.12` against `0.215`).
- It rolls out from the row inside an `overflow: hidden` wrapper: the wrapper
  animates `max-height` 0 → 240px over 200ms and `detail-out` runs it back, so
  the rows below are pushed down as the panel reveals instead of jumping a
  whole panel height at once. The collapse keeps the panel mounted for 200ms
  via `set_timeout` before the view removes it, so the exit is animated too;
  re-opening inside that window clears `closing` and cancels the timeout. The
  panel's background is one step under the rows (`0.12`). Its top shadow is a
  12px background gradient, not an inset `box-shadow`, because this engine
  paints only the first shadow per box and the 16px strip that runs under the
  scrollbar is that one.
- `Remove` is a bordered destructive button in a footer at the bottom of the
  panel, behind a hairline, not a floating red label at the top: the top-right
  corner already means "collapse" to anyone who reads an X there. `Pin`/`Unpin`
  sits at the other end of the same footer. Neither is an icon yet because
  zgui-ui has no icon set wired into this crate.

## Reference points

- Desktop-first, tool-like references: DaVinci Resolve for "quiet surface,
  content wins"; Blender/KDE/JetBrains for density and panel thinking.
- 90s desktop GUIs for square controls, deliberate colour, and texture, but
  not as a literal pixel-art recreation.
- Reject mobile/web-first design systems (Material, Carbon, Spectrum) as the
  basis; they are not the target feel.

## Iteration loop

Visual changes do not need a rebuild. `app::run` reads three env vars at
startup: `SIPHON_SHEET` and `SIPHON_THEME_CSS` replace the compiled-in CSS text,
and `SIPHON_FONT_UI` / `SIPHON_FONT_BRAND` register local font files under the
families `UIFont` and `BrandFont`. A checkpoint is therefore: edit the dev CSS,
restart the binary, screenshot. Only view-structure or data-model changes need
a compile.

## Open

- Exact palette values (semantic colours and the neutral ramp).
- Which texture/pattern, if any, and where.
- Whether `Chillax` needs a local asset path or Fontshare API resolution at
  build time, given the licence's redistribution terms.
