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

- **Archivo** variable font for all UI text. Downloaded to
  `/tmp/opencode/fonts/Archivo.ttf`; axes `wght` 100–900 and `wdth` 62–125.
  ZGUI supports variable fonts through `font-variation-settings`; `font-weight`
  alone does not drive the axes. The narrow `wdth` axis is useful for dense
  table rows.
- **Chillax** for the top "Siphon" wordmark only. Downloaded from Fontshare to
  `/tmp/opencode/fonts/chillax-extract/.../Chillax-Variable.ttf`. It is under
  the ITF Free Font License: embedding in a desktop app is allowed, but the
  font files may not be modified/subset or redistributed through a repository.
  Keep it unmodified and out of git unless the licence is reviewed again.
- Keep the top "Siphon" text even though it is redundant; it looks good.

## Channels view

- The channels table must **fill the entire width** of the content area. It
  currently reads as a small list floating on an empty background.
- Remove the visual gap between the tab bar and the channels content. Add a
  full-width divider/line spanning the whole window between them.
- Alternate row colours (zebra striping) so the dense list remains scannable.
- Keep the row's avatar/display name and the live dot; those are the primary
  identity and state.
- Row hover/open/pending states should be subtle background changes, not
  rounded cards.

## Expanded channel detail

- Do not repeat the avatar and display name from the row. Show only the Twitch
  handle (`@login`) in the detail header.
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
