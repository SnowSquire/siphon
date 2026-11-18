# UI iteration screenshots

Copies of the screenshots from the first visual pass, kept in the repo so they
can be opened directly instead of through the ephemeral kwin-mcp temp dir.

- `01-channels.png`, `02-detail.png`, `03-logs.png` — the stable "before fix"
  set, showing the amber palette that read far too close to Pornhub and the
  chunky filled toggles.
- `04-channels-collapsed.png`, `05-channels-expanded.png`, `06-logs.png` —
  fresh captures of the current shipped build after the dark-row / detached
  detail pass.
- `raw/` — every capture from the later tab-switching session, including the
  intermediate `frame_*.png` files. Tab clicks lagged by a frame on this
  desktop, so some raw filenames do not match the tab visible in the image.
- `dev/` — the external CSS/theme files and launch script used for the
  no-rebuild iteration loop. `launch.sh` still points at the original `/tmp`
  font paths; it is a reference, not a runnable script from this directory.

The final shipped look is compiled into `crates/ui/src/app.rs` (the `SHEET` and
`THEME_CSS` constants), so none of these files are needed to build or run the
UI.
