//! The window styling: the zgui-ui theme tokens and the application sheet.
//!
//! `THEME_CSS` is the token block every zgui-ui control reads; `SHEET` is the
//! hand-written CSS for the classes in `app.rs`. Colours the sheet reuses are
//! declared once in its `:root` palette and referenced through `var()`.

use zgui::prelude::*;

/// The token block the zgui-ui controls read. It is laid over the dark preset
/// rather than written as a second style sheet so the provider owns the cascade
/// and every control is themed by one palette.
pub const THEME_CSS: &str = r"
    --zui-color-background: oklch(0.16 0.012 240);
    --zui-color-foreground: oklch(0.94 0.008 240);
    --zui-color-card: oklch(0.21 0.015 240);
    --zui-color-card-foreground: oklch(0.94 0.008 240);
    --zui-color-popover: oklch(0.23 0.015 240);
    --zui-color-popover-foreground: oklch(0.94 0.008 240);
    --zui-color-primary: oklch(0.75 0.13 230);
    --zui-color-primary-foreground: oklch(0.28 0.05 230);
    --zui-color-secondary: oklch(0.24 0.016 240);
    --zui-color-secondary-foreground: oklch(0.94 0.008 240);
    --zui-color-muted: oklch(0.23 0.016 240);
    --zui-color-muted-foreground: oklch(0.68 0.018 240);
    --zui-color-accent: oklch(0.26 0.018 240);
    --zui-color-accent-foreground: oklch(0.94 0.008 240);
    --zui-color-destructive: #f43f5e;
    --zui-color-destructive-foreground: #ffffff;
    --zui-color-border: oklch(0.32 0.020 240);
    --zui-color-input: oklch(0.32 0.020 240);
    --zui-color-ring: oklch(0.75 0.13 230);
    --zui-color-tooltip: oklch(0.23 0.016 240);
    --zui-color-tooltip-foreground: oklch(0.94 0.008 240);
    --zui-color-tooltip-border: oklch(0.32 0.020 240);
    --zui-radius-base: 0px;
    /* The switch/checkbox components declare their own transition-property and
       read this token; Terminal sets it to 0ms. Overriding the token is what
       actually makes the stock transitions run. */
    --zui-motion-duration-normal: 160ms;
    --zui-motion-ease-standard: ease;
    /* A slim bar in a permanently reserved right strip: the engine cannot do
       `scrollbar-gutter`, so the channels viewport asks for a gutter always. */
    --zui-scrollbar-width: 8px;
";

pub const SHEET: &str = css!(
    r####"
:root {
    /* One palette for the sheet. Every colour used more than once is declared
       here and referenced through var(); the zgui-ui tokens come from
       THEME_CSS, which owns the widget palette. */
    --fg: oklch(0.95 0.008 240);
    --fg-body: oklch(0.88 0.010 240);
    --fg-muted: oklch(0.70 0.018 240);
    --fg-faint: oklch(0.62 0.020 240);
    --fg-dim: oklch(0.56 0.020 240);
    --fg-disabled: oklch(0.45 0.020 240);
    --fg-subtle: oklch(0.50 0.020 240);
    --surface-panel: oklch(0.20 0.013 240);
    --surface-hover: oklch(0.25 0.016 240);
    --border-soft: oklch(0.30 0.018 240);
    --live-bg: #3a1420;
    --live-fg: #ff6b81;
    --error-fg: #ff9aa8;
    background-color: var(--zui-color-background);
    color: var(--fg);
    }

    .app {
        width: 100%;
        height: 100%;
    }

    .header {
        align-items: center;
        gap: 12px;
    }

    .banner {
        align-items: center;
        gap: 8px;
        padding: 4px 8px;
        border-radius: 0;
        background-color: #1c2a1e;
        color: #b9e4bd;
        font-size: 13px;
    }

    .error {
        align-items: center;
        gap: 8px;
        padding: 8px 10px;
        border-radius: 0;
        background-color: #3a1d22;
        color: #f0b8be;
        font-size: 13px;
    }

    .error__text { flex-grow: 1; }

    .tabs {
        flex-shrink: 0;
        gap: 0;
        align-items: center;
        /* Full-bleed rule under the tabs: the chrome above is inset, the
           divider between it and the content is not. */
        margin: 0 -12px;
        padding: 0 14px;
        border-bottom: 1px solid var(--zui-color-border);
    }

    .tab {
        padding: 5px 10px;
        /* The count is placed against the tab's corner rather than in its
           flow, so it cannot push the label off-centre or widen the tab. */
        position: relative;
        border-radius: 10px 10px 0 0;
        font-family: BrandFont, Chillax, sans-serif;
        font-variation-settings: "wght" var(--tab-wght, 200), "wdth" 92;
        border: 1px solid transparent;
        border-bottom: 0;
        color: var(--fg-muted);
        font-size: 13px;
        cursor: pointer;
    }

    .tab:hover {
        color: var(--fg);
        background-color: var(--surface-hover);
    }

    .tab.on {
        color: #ffffff;
        background-color: var(--zui-color-secondary);
        border-color: var(--border-soft);
    }

    .tab--off {
        color: var(--fg-disabled);
        cursor: not-allowed;
    }

    .tab--off:hover {
        color: var(--fg-disabled);
        background-color: transparent;
    }

    .tabbody {
        flex-grow: 1;
        /* Let the body shrink below its content: the tab's own scroll area is
           the thing that gives, not the header and the tab row above it. */
        min-height: 0;
        gap: 10px;
        padding: 12px;
        border-radius: 12px;
        border: 1px solid var(--border-soft);
        background-color: oklch(0.19 0.012 240);
    }

    .tablehead {
        gap: 10px;
        align-items: center;
        padding: 6px 10px;
        /* The cells end 32px from the edge: the 16px the viewport reserves for
           the scrollbar, the cells' own 10px, and the 6px the list used to
           keep. The rows paint the gutter strip with a shadow, so the header
           ends where the cells do, not where the rows do. */
        padding-right: 32px;
        border-left: 2px solid transparent;
        border-bottom: 1px solid var(--zui-color-border);
    }

    .tablehead .cell {
        color: var(--fg-faint);
        font-size: 10px;
        font-weight: 600;
        letter-spacing: 0.6px;
        white-space: nowrap;
    }

    .cell {
        font-size: 12px;
        color: var(--fg-body);
    }

    .cell--chevron { width: 16px; }
    .cell--dot { width: 10px; }
    /* A Twitch login is at most 25 characters; the avatar and the gap take
       the rest of this column, and the name truncates rather than widening
       the row. */
    .cell--channel {
        width: 230px;
        flex-shrink: 0;
        min-width: 0;
    }

    /* Base zero, grow to the gap left by the fixed columns: a long title
       truncates inside its own cell instead of pushing Notify to the right. */
    .cell--title {
        width: 0;
        flex-grow: 1;
        flex-basis: 0;
        min-width: 0;
        overflow: hidden;
    }
    .cell--notify {
        width: 78px;
        display: flex;
        flex-direction: row;
        align-items: center;
        gap: 10px;
    }
    /* Column group over the two switches: the labels are read once here
       instead of being repeated on every row. */
    .notify-head {
        flex-direction: column;
        align-items: center;
        justify-content: center;
        gap: 0;
    }
    .notify-head__group {
        font-size: 10px;
    }
    .notify-head__labels { gap: 10px; }
    .notify-head__label {
        width: 34px;
        font-size: 10px;
        font-weight: 600;
        letter-spacing: 0.6px;
        text-align: center;
        color: var(--fg-faint);
    }
    .tablehead .cell--viewers { text-align: right; }
    .cell--record {
        width: 62px;
        display: flex;
        align-items: center;
    }
    .cell--viewers { width: 92px; text-align: right; }
    .cell--action { width: 34px; }
    .cell--quality { width: 90px; }
    .cell--state { width: 82px; }
    .cell--position { width: 90px; }
    .cell--segments { width: 88px; }
    .cell--size { width: 90px; }
    .cell--runs { width: 56px; }

    .list {
        gap: 0;
        padding: 0;
    }

    .channels, .recordings, .words, .logs {
        flex-grow: 1;
        min-height: 0;
    }

    .channel {
        padding: 0;
    }

    .channel-row {
        align-items: center;
        gap: 10px;
        /* 16px on the right is the scroll gutter the viewport reserves: the
           cells keep clear of it, and `box-shadow` paints that strip in the
           row's own colour without changing the layout box. */
        padding: 6px 16px 6px 10px;
        border-radius: 0;
        /* A transparent 2px edge on every row keeps the cells aligned with the
           header; an open row paints it in the channel's colour. */
        border-left: 2px solid transparent;
        min-height: 30px;
        background-color: oklch(0.145 0.011 240);
        box-shadow: 16px 0 0 0 oklch(0.145 0.011 240);
    }

    .channel-row.pending {
        opacity: 0.65;
    }

    .live-dot {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        width: 10px;
    }

    /* A drawn box, not a text glyph: in the pinned zgui a scaled glyph drops
       out except at rest. */
    .live-dot__lamp {
        width: 6px;
        height: 6px;
        background-color: oklch(0.40 0.015 240);
    }

    .live-dot.live .live-dot__lamp {
        background-color: var(--zui-color-destructive);
        animation: live-pulse 1500ms ease-in-out infinite;
    }

    @keyframes live-pulse {
        0%, 100% { opacity: 1; transform: scale(1); }
        50% { opacity: 0.55; transform: scale(0.8); }
    }

    .identity {
        align-items: center;
        gap: 8px;
        flex-grow: 1;
        min-width: 0;
    }

    .channel-name {
        flex-grow: 1;
        min-width: 0;
        font-family: BrandFont, Chillax, sans-serif;
        font-variation-settings: "wght" 600, "wdth" 100;
        font-size: 14px;
        font-weight: 600;
        color: var(--fg);
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    }

    .channel-title {
        width: 100%;
        height: 18px;
        font-size: 13px;
        color: var(--fg-muted);
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    }

    /* The title cell carries the row's state marks in front of the text: a
       failure, a recording, and a pin. They are small enough to read as part
       of the title rather than as extra columns. */
    .title-cell {
        align-items: center;
        gap: 6px;
        width: 100%;
        min-width: 0;
    }

    .mark {
        display: inline-flex;
        align-items: center;
        justify-content: center;
        width: 12px;
        height: 12px;
        flex-shrink: 0;
        border: 1px solid oklch(0.55 0.14 20);
    }

    .mark__glyph {
        font-size: 9px;
        font-weight: 700;
        line-height: 1;
        color: var(--error-fg);
    }

    .rec-pill {
        flex-shrink: 0;
        padding: 0 4px;
        font-size: 9px;
        font-weight: 700;
        letter-spacing: 0.5px;
        line-height: 13px;
        color: oklch(0.86 0.13 75);
        border: 1px solid oklch(0.45 0.08 75);
        background-color: oklch(0.22 0.04 70);
    }

    /* A pushpin drawn as a head and a stem: a glyph would need a font that
       has one, and this stays crisp at 8px. */
    .pin {
        display: inline-flex;
        flex-direction: column;
        align-items: center;
        gap: 1px;
        width: 8px;
        flex-shrink: 0;
    }

    .pin__head {
        width: 8px;
        height: 3px;
        background-color: var(--zui-color-primary);
    }

    .pin__stem {
        width: 2px;
        height: 4px;
        background-color: var(--zui-color-primary);
    }

    .viewers__value {
        font-size: 12px;
        color: var(--fg-body);
    }

    .viewers__value.stale { color: oklch(0.60 0.015 240); }

    /* The tooltip trigger is an inline-flex box, so `text-align` on the cell
       never reaches its text. Push the one child to the end instead. */
    .viewers-cell { justify-content: flex-end; }

    /* The panel is part of the row, not a card under it: no gap, no top
       border, and the open row's channel-coloured edge runs straight through
       it. The wrapper clips the reveal and animates its own height, so the
       panel rolls out from under the row and pushes the rows below it down
       instead of those rows jumping a whole panel height at once. */
    .detail-clip {
        overflow: hidden;
        animation: detail-in 200ms ease-out;
        /* The panel's 16px strip under the scrollbar. It sits on the wrapper
           because the wrapper's `overflow: hidden` clips a shadow on
           `.detail` itself; its own shadow is not clipped. */
        box-shadow: 16px 0 0 0 oklch(0.12 0.010 240);
    }

    /* Leaving: the same roll in reverse, held until the row unmounts it. */
    .detail-clip.closing {
        animation: detail-out 200ms ease-in forwards;
    }

    .detail {
        margin: 0;
        padding: 10px 16px 10px 10px;
        gap: 12px;
        border: 0;
        border-left: 2px solid var(--channel-color, var(--zui-color-primary));
        border-radius: 0;
        /* One step under the rows, close to the table's own surface: the open
           row casts into it, so the panel reads as a well the row opened
           rather than a card laid under it. The gradient is the drop shadow:
           this engine paints only the first shadow in a list and the wrapper
           already spends its one on the gutter strip. */
        background-color: oklch(0.12 0.010 240);
        background-image: linear-gradient(rgba(0, 0, 0, 0.5), rgba(0, 0, 0, 0));
        background-size: 100% 12px;
        background-repeat: no-repeat;
        background-position: top;
    }

    /* The content is at most a wrapped title plus the grid, so 240px is a
       ceiling that never clips the settled panel; the reveal finishes when
       the used height reaches the content and the rest of the curve is flat. */
    @keyframes detail-in {
        from { max-height: 0; }
        to { max-height: 240px; }
    }

    @keyframes detail-out {
        from { max-height: 240px; }
        to { max-height: 0; }
    }

    .detail-foot {
        align-items: center;
        padding-top: 8px;
        border-top: 1px solid oklch(0.28 0.016 240);
    }

    /* The component's own scoped rules are what set a button's tone, so the
       override has to be more specific than :scope[data-variant]. */
    .app .detail-foot .detail-remove {
        color: var(--error-fg);
        border-color: oklch(0.40 0.06 20);
    }

    .app .detail-foot .detail-remove:hover {
        color: #ffd3d9;
        border-color: oklch(0.48 0.08 20);
        background-color: oklch(0.26 0.035 20);
    }

    .detail-head {
        align-items: center;
        gap: 12px;
    }

    .detail-login {
        font-size: 12px;
        color: var(--fg-faint);
    }

    .badge {
        padding: 3px 10px;
        border-radius: 999px;
        font-size: 10px;
        font-weight: 700;
        letter-spacing: 0.6px;
        background-color: var(--zui-color-secondary);
        color: var(--fg-faint);
    }

    .badge.live {
        background-color: var(--live-bg);
        color: var(--live-fg);
    }

    .detail-grid {
        gap: 24px;
        padding-top: 4px;
    }

    .detail-col {
        flex-grow: 1;
        flex-basis: 0;
        gap: 5px;
    }

    .detail-item {
        gap: 10px;
        align-items: baseline;
    }

    .detail-key {
        width: 96px;
        font-size: 12px;
        color: var(--fg-faint);
    }

    .detail-value, .detail-value__text {
        font-size: 12px;
        color: var(--fg-body);
    }
    .detail-value { flex-grow: 1; }

    .settings { gap: 8px; }

    .switch-row {
        align-items: center;
        gap: 12px;
        padding: 12px 14px;
        border-radius: 10px;
        border: 1px solid var(--border-soft);
        background-color: var(--surface-panel);
    }

    .switch-copy { gap: 2px; }

    .switch-title {
        font-size: 14px;
        font-weight: 600;
    }

    .switch-hint {
        font-size: 12px;
        color: var(--fg-faint);
    }

    .section-title {
        padding-top: 6px;
        font-size: 11px;
        font-weight: 700;
        letter-spacing: 0.7px;
        color: var(--fg-muted);
    }

    .section-hint {
        font-size: 12px;
        color: var(--fg-dim);
    }

    .add {
        gap: 8px;
        align-items: center;
        padding-top: 2px;
    }

    .add__input { flex-grow: 1; }
    .add__quality { width: 170px; }

    .word {
        align-items: center;
        gap: 8px;
        padding: 6px 10px;
        border-radius: 8px;
    }

    .word:hover, .recording-row:hover, .notification:hover,
    .pipeline:hover, .log:hover { background-color: oklch(0.26 0.017 240); }

    .word__text {
        font-size: 13px;
        color: var(--fg-body);
    }

    .recording { padding: 0; }

    .recording-row {
        align-items: center;
        gap: 10px;
        padding: 8px 10px;
        border-radius: 9px;
    }

    .recording__error {
        padding: 0 10px 6px 10px;
        font-size: 11px;
        color: var(--error-fg);
    }

    .empty {
        padding: 22px 12px;
        font-size: 13px;
        color: var(--fg-faint);
    }

    .stats { gap: 8px; }

    .stat {
        flex-grow: 1;
        flex-basis: 0;
        gap: 2px;
        padding: 10px 12px;
        border-radius: 10px;
        border: 1px solid var(--border-soft);
        background-color: var(--surface-panel);
    }

    .stat__label {
        font-size: 10px;
        font-weight: 600;
        letter-spacing: 0.6px;
        color: var(--fg-faint);
    }

    .stat__value {
        font-size: 15px;
        font-weight: 600;
        color: var(--fg);
    }

    .loglist { gap: 1px; }

    .notification {
        align-items: flex-start;
        gap: 10px;
        padding: 6px 10px;
        border-radius: 8px;
    }

    .notification__body { gap: 1px; flex-grow: 1; }

    .notification__summary {
        font-size: 13px;
        font-weight: 600;
        color: var(--fg);
    }

    .notification__text {
        font-size: 12px;
        color: var(--fg-muted);
    }

    .pipeline {
        align-items: center;
        gap: 10px;
        padding: 6px 10px;
        border-radius: 8px;
    }

    .pipeline__name {
        width: 220px;
        font-size: 13px;
        font-weight: 600;
    }

    .pipeline__state {
        width: 74px;
        font-size: 12px;
        color: var(--fg-muted);
    }

    .pipeline__meta {
        font-size: 12px;
        color: var(--fg-faint);
    }

    .pipeline__rec {
        display: none;
        padding: 1px 8px;
        border-radius: 999px;
        background-color: var(--live-bg);
        color: var(--live-fg);
        font-size: 10px;
        font-weight: 700;
    }

    .pipeline__rec.on { display: inline-flex; }

    .log {
        align-items: baseline;
        gap: 10px;
        padding: 5px 10px;
        border-radius: 0;
    }

    .log__time {
        width: 58px;
        font-size: 11px;
        color: var(--fg-dim);
    }

    .log__target {
        width: 74px;
        font-size: 11px;
        color: var(--zui-color-primary);
    }

    .log__message {
        flex-grow: 1;
        font-size: 12px;
        color: var(--fg-body);
    }

    .log.bad .log__message { color: var(--error-fg); }

    .log__meta {
        font-size: 11px;
        color: var(--fg-faint);
    }

:root {
    font-family: UIFont, Archivo, SiphonEmoji, sans-serif;
    font-variation-settings: "wght" 400, "wdth" 92;
}

/* Each label keeps a fixed box, measured once for Chillax at the selected
   weight so the `wght` tween cannot resize the tab or shift the row. The text
   child is centred by the box, so a fallback font that measures wider spills
   equally on both sides instead of hanging off the right. The string is an
   explicit child rather than the box's own text: a flex container's anonymous
   text item is mis-shaped by this zgui revision once the font variation
   changes. */
.tab__label {
    display: flex;
    align-items: center;
    justify-content: center;
    width: var(--tab-label-w, 60px);
}

.tab__text { white-space: nowrap; }
/* A count in the tab's corner. It is out of flow so it can neither widen the
   tab nor push its label off-centre; only its opacity changes with the count. */
.tab__count {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    position: absolute;
    top: 3px;
    right: 3px;
    min-width: 12px;
    height: 12px;
    padding: 0 2px;
    font-family: UIFont, Archivo, SiphonEmoji, sans-serif;
    font-size: 9px;
    font-weight: 700;
    line-height: 1;
    opacity: 0;
}
.tab__count.on { opacity: 1; }
.tab__count--error { background-color: oklch(0.45 0.16 20); color: #ffffff; }
.tab__count--rec { background-color: oklch(0.58 0.13 75); color: oklch(0.18 0.03 70); }
.tabbody {
    min-height: 0;
    gap: 0;
    padding: 0;
    border: 0;
    border-radius: 0;
    background-color: transparent;
}
.channels {
    background-color: oklch(0.11 0.010 240);
}
/* The bar lives in a permanent right strip. `scrollbar-gutter` is unsupported
   here, so `overflow: scroll` keeps the gutter reserved whether or not the
   rows overflow; expanding a row can no longer pull the columns sideways. */
.channels .zui-scroll-area__viewport {
    overflow-y: scroll;
    overflow-x: hidden;
    /* The engine's thumb is painted under the rows, so `ListThumb` draws the
       visible one after them; this keeps the hidden one from showing through
       the empty area below the last row. */
    --zgui-scrollbar-thumb: transparent;
}
.list { gap: 0; padding: 0; }

/* The visible thumb `ListThumb` draws: the engine's position, the engine's
   look (a slim light capsule centred in the gutter), after the rows. */
.list-thumb {
    position: absolute;
    top: 0;
    right: 5px;
    width: 6px;
    border-radius: 3px;
    background-color: rgba(255, 255, 255, 0.5);
    pointer-events: none;
}

.list-thumb.hidden { display: none; }

/* Zebra rows; hover/open must follow the even-row rule to win. Each one also
   carries the 16px strip that paints under the scrollbar in its own colour. */
.channel:nth-child(even) .channel-row {
    background-color: oklch(0.165 0.012 240);
    box-shadow: 16px 0 0 0 oklch(0.165 0.012 240);
}
.channel-row:hover {
    background-color: oklch(0.19 0.014 240);
    box-shadow: 16px 0 0 0 oklch(0.19 0.014 240);
}
.channel-row.open, .channel:nth-child(even) .channel-row.open {
    background-color: oklch(0.215 0.014 240);
    box-shadow: 16px 0 0 0 oklch(0.215 0.014 240);
    border-left-color: var(--channel-color, var(--zui-color-primary));
}
/* A live row is full weight; offline rows step back so the live ones carry
   the eye without introducing another accent. */
.channel-row.offline .channel-name { color: oklch(0.78 0.010 240); }
.channel-row.offline .channel-title__run { color: oklch(0.56 0.014 240); }
.channel-title__run {
    display: inline-block;
    white-space: nowrap;
    line-height: 18px;
    font-size: 12px;
    color: var(--fg-muted);
}
/* The row truncates the title to keep Notify aligned; the expanded panel is
   where the whole thing goes, wrapped rather than clipped. */
.detail-title {
    width: 100%;
    font-size: 13px;
    line-height: 1.4;
    color: var(--fg);
    white-space: normal;
    overflow-wrap: anywhere;
}
.detail-error {
    width: 100%;
    font-size: 12px;
    line-height: 1.4;
    color: var(--error-fg);
    white-space: normal;
    overflow-wrap: anywhere;
}

/* A bordered box instead of a text glyph: zgui does not paint text inside a
   transformed element, but it does transform boxes. The tip is a bottom-right
   corner, so -45deg points right and +45deg points down when the row is open.
   The outer box stretches to the row and centres the tip vertically. */
.chevron {
    display: inline-flex;
    align-self: stretch;
    align-items: center;
    justify-content: center;
    width: 16px;
}
.chevron__tip {
    width: 6px;
    height: 6px;
    border-right: 1px solid var(--fg-subtle);
    border-bottom: 1px solid var(--fg-subtle);
    transform: rotate(-45deg);
    transition-property: transform, border-color;
    transition-duration: 160ms;
    transition-timing-function: ease;
}
.chevron.open .chevron__tip {
    border-color: var(--channel-color, var(--zui-color-primary));
    transform: rotate(45deg);
}

/* The stock switch is a rounded pill, which is the wrong shape and too small
   for a dense desktop tool. Rebuild it as a hard-edged track with a square
   thumb on the same 4px grid. The checked colour comes from --channel-color
   (the channel's Twitch chat colour, with a generated fallback); the global
   settings have no channel, so they fall through to the app's primary. The
   extra class in each selector outranks the component's own scoped sheet. */
.app .zui-switch,
.app .zui-switch.zui-switch--sm {
    width: 40px;
    height: 22px;
    border: 1px solid var(--zui-color-border);
    border-radius: 0;
    background-color: var(--zui-color-control-switch-off);
    box-shadow: none;
}
/* The component's small size is a class in this revision, not a data
   attribute; the dense rows are the only place that asks for it. */
.app .zui-switch.zui-switch--sm {
    width: 34px;
    height: 18px;
}
.app .zui-switch:checked {
    background-color: var(--channel-color, var(--zui-color-primary));
    border-color: var(--channel-color, var(--zui-color-primary));
}
.app .zui-switch > .zui-switch__thumb,
.app .zui-switch.zui-switch--sm > .zui-switch__thumb {
    width: 18px;
    height: 18px;
    border-radius: 0;
    margin-left: 2px;
    margin-right: 2px;
    background-color: var(--zui-color-control-switch-thumb);
    transform: translateX(0);
    transition-property: transform;
    transition-duration: var(--zui-motion-duration-normal);
    transition-timing-function: var(--zui-motion-ease-standard);
}
.app .zui-switch.zui-switch--sm > .zui-switch__thumb {
    width: 14px;
    height: 14px;
}
.app .zui-switch:checked > .zui-switch__thumb {
    background-color: var(--zui-color-control-switch-thumb-on);
    transform: translateX(16px);
}
.app .zui-switch.zui-switch--sm:checked > .zui-switch__thumb {
    transform: translateX(14px);
}
/* Only translate is animated: in this pinned zgui, glyphs under a scale
   transform are dropped except at the rest keyframe. Eight copies sliding by
   one copy (-12.5%) keep the loop seamless and the window filled. */
.app { padding: 14px 12px 10px; gap: 6px; }
.header {
    height: 30px;
    /* Chrome does not give way to content: the body's scroll area does. */
    flex-shrink: 0;
    min-height: 0;
    padding: 0 10px;
    background-color: oklch(0.12 0.010 240);
    border-bottom: 1px solid var(--zui-color-border);
    overflow: hidden;
}
.tabs .conn {
    flex: 0 0 auto;
    padding: 0 6px;
    color: oklch(0.80 0.012 240);
    font-size: 12px;
}
.ticker {
    flex: 1 1 auto;
    min-width: 0;
    height: 100%;
    display: flex;
    align-items: center;
    overflow: hidden;
}
.ticker__run {
    display: inline-block;
    white-space: nowrap;
    line-height: 1;
    font-size: 12px;
    color: oklch(0.68 0.018 240);
    animation: ticker-scroll var(--ticker-duration, 40s) linear infinite;
}
/* The toast band is portalled at the window corner, which puts it over the
   add bar. The region has no transform of its own, so lifting the whole stack
   clears that row without touching the slots' own transforms. */
.toast-lift { transform: translateY(-52px); }
@keyframes ticker-scroll {
    from { transform: translateX(0); }
    to { transform: translateX(-12.5%); }
}
"####
);
