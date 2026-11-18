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
  hues. Semantic reds and per-channel colours are the only other hues.
- The channels scrollbar lives in a permanent right strip: `--zui-scrollbar-width`
  is 8px, the viewport uses `overflow: scroll` so the gutter is always reserved
  (`scrollbar-gutter` is unsupported in ZGUI), and the list keeps 6px of right
  padding. Expanding a row cannot pull the columns sideways.
- Motion is back for two controls: the switch track/thumb transition over
  160ms, and a live dot pulses while the channel is live. The switch gets its
  motion from `--zui-motion-duration-normal`: zgui-ui's switch declares its own
  `transition-property` and reads that token, and the Terminal preset sets it
  to `0ms`, so the token is overridden in `THEME_CSS` rather than redeclaring
  transitions in the app sheet. The live dot is a drawn box, not a glyph: at
  the pinned zgui revision a translated glyph animates and paints fine, but a
  `scale()` transform drops the glyph except at its rest keyframe (verified on
  the live Wayland window with kwin-mcp). Only translation is animated in the
  app. The thumb keeps a 2px inset at both ends of its travel. Hover
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
- **Chillax** (Fontshare, ITF Free Font License 2.0) is used for channel names
  via the `BrandFont` family; download the unmodified variable TTF from
  `https://api.fontshare.com/v2/fonts/download/chillax` to
  `/tmp/opencode/fonts/Chillax-Variable.ttf` and pass it as `SIPHON_FONT_BRAND`.
  The licence allows embedding in the app but explicitly forbids distributing
  the font file through a repository, so it is **not** in git; release builds
  need a build-time fetch or a separately licensed asset. `font-family:
  BrandFont, Chillax, sans-serif` degrades to the system font when the dev
  override is absent.
- **Noto Color Emoji** (CBDT build, OFL 1.1) ships in `crates/ui/fonts/` and is
  registered as `SiphonEmoji`. It is both the last entry in the root
  `font-family` stack and bound to fontique's `Emoji` generic. The CBDT build
  matters: the system COLRv1 font decodes as a monochrome outline in zgui's
  swash 0.2.10. Verified with kwin-mcp that 😀/🎉/✅ render in colour. `★` and
  `✧` are not Unicode-emoji, so they stay monochrome by design.

## Header / activity ticker

- The compositor's title bar already carries "Siphon", so the in-app wordmark
  was redundant. Its row is now a news ticker over the daemon's notification
  stream: newest first, each item `summary — body`, separated by `│`.
- The run repeats four times and slides by `-25%` (one copy) on a linear loop
  whose duration scales with the text length. Four copies keep the window full
  even when there is only one short notification. Connection health lives at
  the right end of the **tab row**, not in the ticker; the update banner still
  sits at the right of the ticker row when present.
- Keep the 14px top padding and the 6px gap below the 30px strip. Deleting the
  space entirely makes the tabs look clipped against the title bar; filling
  it with live activity is what makes the gap read as deliberate.
- Do not use `scale()` on the ticker text (see the transform note above).

## Channels view

- The channels table must **fill the entire width** of the content area. It
  currently reads as a small list floating on an empty background.
- Remove the visual gap between the tab bar and the channels content. Add a
  full-width divider/line spanning the whole window between them.
- Alternate row colours (zebra striping) so the dense list remains scannable.
  The neutral ramp is a three-step one: empty table background `#04060a`,
  odd/dark rows `#0b0d12`, even rows `#10131a`. The chrome/root background is
  `#080a10`. The dark rows need their own explicit colour — if they are left
  transparent they inherit the table background and the banding disappears
  below the last row.
- Keep the row's avatar/display name and the live dot; those are the primary
  identity and state. Display names use Chillax (see Typography).
- Titles longer than 55 characters repeat three times and scroll by `-33.333%`
  **on row hover only** (`animation-play-state: paused` until hover), so a
  dense list does not have every row moving at once.
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
