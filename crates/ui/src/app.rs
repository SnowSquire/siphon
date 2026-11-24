//! The ZGUI view: header, tabs, channels, filters, recordings, logs, and the
//! renderer setup.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use siphon_shared::riffle::ArenaSummary;
use siphon_shared::riffle::ChannelView;
use siphon_shared::riffle::ConnHealth;
use siphon_shared::riffle::DownloaderSummary;
use siphon_shared::riffle::LogEntry;
use siphon_shared::riffle::LogLevel;
use siphon_shared::riffle::PipelineState;
use siphon_shared::riffle::PipelineView;
use siphon_shared::riffle::Request;
use siphon_shared::settings::ClientSettings;
use siphon_shared::settings::HdrMode;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::reactive::Signal;
use zgui::reactive::UnsyncCallback;
#[cfg(windows)]
use zgui::runtime::use_window;
#[cfg(windows)]
use zgui::runtime::use_windows;
use zgui::vocab::Key;
use zgui::vocab::NamedKey;
use zgui_ui::prelude::*;
use zgui_ui_tokens::prelude::*;

use crate::css::SHEET;
use crate::css::THEME_CSS;
use crate::format::format_ago;
use crate::format::format_bytes;
use crate::format::format_count;
use crate::format::format_duration_ms;
use crate::format::format_iso_ms;
use crate::format::format_time_ms;
use crate::format::format_uptime;
use crate::riffle::IntentSender;
use crate::riffle::UiEvent;
use crate::riffle::UiState;
use crate::settings::Display;
#[cfg(windows)]
use crate::tray::TrayAction;

/// Which tab the window shows. `Watch` is deliberately absent: the video
/// surface is not implemented, and its tab is drawn but never selected.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Channels,
    Filter,
    Recordings,
    Logs,
    Settings,
}

/// Writes the wall time of every frame that took more than a few milliseconds
/// to stderr, which the app log captures. `SIPHON_FRAME_LOG` turns it on; it
/// exists to measure a hitch, not to ship in a window.
///
/// The `gap` is the time since the previous frame ended: a stall that blocks
/// the UI thread between frames shows up there, not in a frame's own `elapsed`.
struct FrameLog {
    last: Cell<Option<std::time::Instant>>,
}

impl zgui::runtime::FrameProbe for FrameLog {
    fn frame_ended(&self, window: &zgui::runtime::Window) {
        let now = std::time::Instant::now();
        let gap = self
            .last
            .get()
            .map_or(0.0, |last| (now - last).as_secs_f64() * 1000.0);
        self.last.set(Some(now));
        let Some(report) = window.last_frame_report() else {
            return;
        };
        let elapsed = report.elapsed.as_secs_f64() * 1000.0;
        let paint = report.paint_elapsed.as_secs_f64() * 1000.0;
        if gap < 30.0 && elapsed < 20.0 && paint < 20.0 {
            return;
        }
        let size = window.surface().size();
        eprintln!(
            "frame gap {gap:.1}ms {elapsed:.1}ms paint {paint:.1}ms {}x{} restyled {} animated {} timers {}",
            size.width.0, size.height.0, report.restyled, report.animated, report.timers_fired
        );
    }
}

pub fn run(events: kanal::Receiver<UiEvent>, intents: IntentSender) -> eros::Result<()> {
    // The settings document comes first: the state and the renderer hold the
    // same handle to it, so a switch writes the value the file keeps.
    let display = Display::load();
    let state = UiState::new(display.settings());
    // The preset dark scheme is close, but the window has its own ink. The
    // token block is what makes every zgui-ui control (switches, inputs,
    // tooltips, buttons) use the same palette as the hand-written classes.
    // Dev loop: both token and sheet text can be overridden from files, so a
    // visual change needs a process restart, not a recompile.
    let theme_css = dev_text("SIPHON_THEME_CSS", THEME_CSS);
    let dark = Signal::stored_local(Theme::dark().with_css(&theme_css));
    let sheet = dev_text("SIPHON_SHEET", SHEET);

    let window = app()
        .with_application_id("com.iken.siphon")
        .with_title("Siphon")
        .with_size(1000.0, 760.0)
        .with_min_size(840.0, 560.0)
        .with_resizable(true)
        // The libplacebo chain owns presentation pacing. ZGUI must keep
        // composing at the animation cadence instead of waiting for a surface
        // callback that the swapchain's own `wl_surface.frame()` request makes
        // arrive one serial late; see `chain::wayland::wait_for_frame`.
        .with_compose_ahead(ComposeAhead::Always)
        .with_stylesheet(sheet);

    let window = if std::env::var_os("SIPHON_FRAME_LOG").is_some() {
        window.with_probe(Rc::new(FrameLog {
            last: Cell::new(None),
        }))
    } else {
        window
    };

    // Shipped fonts are registered by path in dev so typography can be tried
    // without a rebuild: SIPHON_FONT_UI, SIPHON_FONT_BRAND. Release builds
    // keep ZGUI's system fonts until the licensing/asset path is settled.
    let fonts = fonts()?;
    let window = window.with_fonts(fonts);

    // Both platforms compose the interface into a texture and present it
    // through a chain: DXGI on Windows, a Vulkan swapchain on Wayland. Only the
    // chain differs, and it is chosen inside the renderer factory.
    //
    let window = {
        let (gpu, _decode) = crate::gpu::create()?;
        let graphics = zgui_render_wgpu::SharedGraphics::with_gpu(Arc::clone(&gpu));
        let factory_gpu = Arc::clone(&gpu);
        let factory_display = Rc::clone(&display);
        window.with_renderer(Box::new(move |surface, target| {
            crate::chain::renderer(&graphics, &factory_gpu, surface, target, &factory_display)
        }))
    };

    window
        .run(move || {
            // Fresh handles per build: the run closure is `FnMut`, so it must
            // not move its captures into the first view it makes.
            let state = state.clone();
            let intents = intents.clone();
            let events = events.clone();
            let display = Rc::clone(&display);
            let dark = dark;
            view! {
                TooltipProvider(delay = Duration::from_millis(400)) {
                    ThemeProvider(scheme = ColorScheme::Dark, dark = dark) {
                        // The toaster wraps the interface rather than sitting
                        // beside it: its queue reaches callers through the scope
                        // tree, and the row that removes a channel is the one
                        // that has to offer the undo.
                        Toaster(
                            corner = ToastCorner::BottomRight,
                            limit = 3,
                            class = "toast-lift"
                        ) {
                            App(state = state, intents = intents, events = events, display = display)
                        }
                    }
                }
            }
        })
        .map_err(|error| eros::error!("zgui: {error}"))
}

/// Reads a dev override for a stylesheet from `path_var`, or the compiled-in
/// text when the variable is unset or the file cannot be read.
fn dev_text(path_var: &str, fallback: &str) -> String {
    std::env::var_os(path_var)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_else(|| fallback.to_owned())
}

/// Registers the shipped fonts, then any dev overrides. Archivo, Chillax and a
/// CBDT Noto Color Emoji are embedded so every platform renders the same text
/// and real colour emoji: the system emoji font can be COLRv1, which zgui's
/// rasterizer decodes as a monochrome outline.
fn fonts() -> eros::Result<Fonts> {
    let fonts = Fonts::system();
    fonts
        .register(
            Arc::new(include_bytes!("../fonts/Archivo-Variable.ttf").to_vec()),
            Some("UIFont"),
        )
        .map_err(|error| eros::error!("shipped UIFont: {error}"))?;
    fonts
        .register(
            Arc::new(include_bytes!(concat!(env!("OUT_DIR"), "/Chillax-Variable.ttf")).to_vec()),
            Some("BrandFont"),
        )
        .map_err(|error| eros::error!("shipped BrandFont: {error}"))?;
    fonts
        .register(
            Arc::new(include_bytes!("../fonts/NotoColorEmoji-CBDT.ttf").to_vec()),
            Some("SiphonEmoji"),
        )
        .map_err(|error| eros::error!("shipped emoji font: {error}"))?;

    // parley appends the Emoji generic to every emoji cluster's font query.
    // Binding it to the shipped family puts that face ahead of the platform
    // font; the collection is shared with the shaper and rasterizer.
    let mut context = fonts.shaper().fonts().font_context();
    if let Some(id) = context.collection.family_id("SiphonEmoji") {
        context
            .collection
            .set_generic_families(fontique::GenericFamily::Emoji, std::iter::once(id));
    }

    for (var, family) in [
        ("SIPHON_FONT_UI", "UIFont"),
        ("SIPHON_FONT_BRAND", "BrandFont"),
    ] {
        let Some(path) = std::env::var_os(var) else {
            continue;
        };
        let bytes =
            std::fs::read(&path).map_err(|error| eros::error!("{var} ({path:?}): {error}"))?;
        let data: zgui::text::FontData = Arc::new(bytes);
        fonts
            .register(data, Some(family))
            .map_err(|error| eros::error!("{var} ({family}): {error}"))?;
    }
    Ok(fonts)
}

/// Gives back the resident pages a hidden window no longer needs.
///
/// What a window makes resident is mostly driver code and work done for the
/// first frames, which is never touched again while it sits in the tray: the
/// measured private working set falls from 134 MB to about 1 MB when a hidden
/// window asks for this, and only a few megabytes fault back when it is shown.
/// Only a hidden window asks: on a visible one the same call would turn the
/// next pointer move into page faults.
#[cfg(windows)]
fn release_hidden_memory() {
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::Win32::System::Threading::SetProcessWorkingSetSize;

    // SAFETY: the pseudo-handle names this process, and the two sizes ask the
    // system to remove as many pages from the working set as it can.
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn App(
    state: UiState,
    intents: IntentSender,
    events: kanal::Receiver<UiEvent>,
    display: Rc<Display>,
) -> impl IntoView {
    let intents = Rc::new(intents);
    // One handle per signal the view reads, so a closure depends on that field
    // alone: ZGUI reruns it when the signals it read change, not when any part of
    // the snapshot does.
    let conn_sig = state.conn;
    let error_sig = state.error;
    let offer = state.update;
    let channels_sig = state.channels;
    let pins_sig = state.pins;
    let pending_sig = state.pending;
    let pipelines_sig = state.pipelines;
    let arena_sig = state.arena;
    let downloader_sig = state.downloader;
    let notices_sig = state.notices;
    let logs_sig = state.logs;
    let settings_sig = state.settings;

    // The tray owns the window's lifetime on Windows: closing it puts it away,
    // and a tray click is what brings it back. Linux has neither, so the close
    // is the framework's default and the process ends with the window.
    #[cfg(windows)]
    {
        let window = use_window();
        let windows = use_windows();

        // Tray clicks run here, on the GUI thread, rather than through the interval:
        // a window hidden to the tray draws no frames, so a click queued for one
        // would wait for the very window it was meant to bring back.
        crate::tray::set_action_handler({
            let window = window.clone();
            let windows = windows.clone();
            move |action| match action {
                TrayAction::Show => {
                    window.set_visible(true);
                    window.focus();
                }
                TrayAction::Quit => windows.quit(),
                TrayAction::Restart => {
                    if let Err(error) = std::thread::Builder::new()
                        .name("service restart".to_owned())
                        .spawn(crate::service::restart)
                    {
                        tracing::error!(
                            target: "service",
                            "could not start the restart thread: {error}"
                        );
                    }
                }
            }
        });

        // Closing the window puts it away in the tray instead of ending the process:
        // the way back is the tray's Open (or a double-click), the way out is Quit
        // there. The backbuffers go back now, because a hidden window is handed no
        // frame that could do it later, and the frame that shows it again grows the
        // chain back.
        let close = on_close_request({
            let window = window.clone();
            move || {
                window.set_visible(false);
                release_hidden_memory();
                CloseResponse::Veto
            }
        });
        // The callback lives as long as the window; the component has no teardown
        // hook that owns the guard.
        std::mem::forget(close);

        // A login autostart passes `--minimized`, and the surface is shown by
        // its first presented frame, so the hide cannot precede it: this runs
        // just after, and the tray's Open is the way back. The handle is
        // forgotten because the timer fires once and nothing can cancel it.
        if std::env::args().any(|argument| argument == "--minimized") {
            let handle = set_timeout(Duration::from_millis(50), move || {
                window.set_visible(false);
                release_hidden_memory();
            });
            std::mem::forget(handle);
        }
    }

    // The RPC worker publishes into this channel, and that send is what wakes the
    // UI thread: the receive below is where the frame comes from, so nothing is
    // polled. Whatever arrived alongside the event is drained in the same frame,
    // which is why a burst costs one repaint rather than one per event.
    zgui::reactive::spawn_detached({
        let state = state.clone();
        async move {
            loop {
                let Ok(event) = events.as_async().recv().await else {
                    break;
                };
                state.apply_event(event);
                while let Ok(Some(event)) = events.try_recv() {
                    state.apply_event(event);
                }
            }
        }
    });

    let ticker = Signal::derive_local(move || ticker_text(&channels_sig.get(), &notices_sig.get()));
    // Relative labels ("2h ago") age on their own clock. Once a minute is
    // enough for the buckets `format_ago` uses; the timer re-arms itself, and
    // nothing else in the interface ticks. The handle has to outlive the call:
    // dropping it is what cancels the timer.
    let now = RwSignal::new_local(crate::format::now_ms());
    {
        fn arm(
            now: RwSignal<i64, LocalStorage>,
            slot: Rc<RefCell<Option<zgui::view::TimeoutHandle>>>,
        ) {
            let next = Rc::clone(&slot);
            let handle = set_timeout(Duration::from_secs(30), move || {
                now.set(crate::format::now_ms());
                arm(now, next);
            });
            *slot.borrow_mut() = Some(handle);
        }
        arm(now, Rc::new(RefCell::new(None)));
    }
    let toggle_pin = {
        let state = state.clone();
        UnsyncCallback::new(move |id: u64| state.toggle_pin(id))
    };
    // A running recording and a channel in trouble are the two things worth
    // seeing without opening the tab that holds them.
    let rec_count = Signal::derive_local(move || {
        pipelines_sig
            .get()
            .iter()
            .filter(|pipeline| pipeline.with(is_recorded))
            .count()
    });
    let error_count = Signal::derive_local(move || {
        let pipelines = pipelines_sig.get();
        channels_sig
            .get()
            .iter()
            .filter(|channel| channel.with_untracked(|c| channel_error(c, &pipelines).is_some()))
            .count()
    });
    let tab_weight = RwSignal::new_local(1.0f32);
    let previous_tab = RwSignal::new_local(None::<Tab>);
    let tab_generation = Rc::new(Cell::new(0u64));
    let tab = RwSignal::new_local(Tab::Channels);
    // The window clock, captured here so an animation started from a pointer or
    // click handler is not owned by that handler's scope.
    let timers = Timers::current();
    let tab_click = {
        let timers = timers.clone();
        move |target: Tab| {
            let generation = Rc::clone(&tab_generation);
            let timers = timers.clone();
            move |_: &mut EventCx<'_, events::Click>| {
                previous_tab.set(Some(tab.get_untracked()));
                tab.set(target);
                if let Some(timers) = timers.as_ref() {
                    animate_tab_weight(timers, tab_weight, Rc::clone(&generation));
                }
            }
        }
    };
    let expanded = RwSignal::new_local(BTreeSet::<u64>::new());
    // Panels that are playing their exit animation: still mounted, no longer
    // "open" in the sense that a click should close them.
    let closing = RwSignal::new_local(BTreeSet::<u64>::new());
    let draft = RwSignal::new_local(String::new());
    let word_draft = RwSignal::new_local(String::new());
    // Owned here because the field is drawn in the tab strip, above the tab's
    // own body; the Channels list reads it through a prop.
    let channel_filter = RwSignal::new_local(String::new());
    let once_login = RwSignal::new_local(String::new());
    let once_quality = RwSignal::new_local(String::new());

    let update = Rc::clone(&intents);
    let dismiss = Rc::clone(&intents);
    let channels_intents = Rc::clone(&intents);
    let recordings_intents = Rc::clone(&intents);
    // One clone per tab: the view's `if` bodies are closures, so each one that
    // uses the state has to own its handle.
    let state_channels = state.clone();
    let state_filter = state.clone();

    view! {
        column(class = "app") {
            row(class = "header") {
                box(class = "ticker") {
                    text(
                        class = "ticker__run",
                        var:--ticker-duration = move || Some(ticker_duration(&ticker.get())),
                    ) {
                        {move || ticker.get()}
                    }
                }
                spacer()
                if move || offer.get().is_some() {
                    row(class = "banner") {
                        text {{move || offer.get().map_or_else(String::new, |update| format!("Update to {}", update.version))}}
                        Button(
                            variant = ButtonVariant::Outline,
                            on:click = {
                                let update = Rc::clone(&update);
                                move |_| {
                                    update.send(Request::ApplyUpdate);
                                }
                            }
                        ) {"Install"}
                    }
                }
            }

            if move || error_sig.get().is_some() {
                row(class = "error") {
                    text(class = "error__text") {{move || error_sig.get().unwrap_or_default()}}
                    spacer()
                    Button(
                        size = ButtonSize::Sm,
                        variant = ButtonVariant::Ghost,
                        on:click = {
                            let dismiss = Rc::clone(&dismiss);
                            move |_| {
                                dismiss.send(Request::ClearError);
                            }
                        }
                    ) {"Dismiss"}
                }
            }

            row(class = "tabs") {
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Channels,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Channels),
                    on:click = tab_click(Tab::Channels)
                ) {
                    box(class = "tab__label", var:--tab-label-w = Some("66px".to_owned())) {
                        text(class = "tab__text") {"Channels"}
                    }
                    box(class = "tab__count tab__count--error", class:on = move || error_count.get() > 0) {
                        {move || if error_count.get() > 0 { error_count.get().to_string() } else { String::new() }}
                    }
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Filter,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Filter),
                    on:click = tab_click(Tab::Filter)
                ) {
                    box(class = "tab__label", var:--tab-label-w = Some("36px".to_owned())) {
                        text(class = "tab__text") {"Filter"}
                    }
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Recordings,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Recordings),
                    on:click = tab_click(Tab::Recordings)
                ) {
                    box(class = "tab__label", var:--tab-label-w = Some("74px".to_owned())) {
                        text(class = "tab__text") {"Recordings"}
                    }
                    box(class = "tab__count tab__count--rec", class:on = move || rec_count.get() > 0) {
                        {move || if rec_count.get() > 0 { rec_count.get().to_string() } else { String::new() }}
                    }
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Logs,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Logs),
                    on:click = tab_click(Tab::Logs)
                ) {
                    box(class = "tab__label", var:--tab-label-w = Some("32px".to_owned())) {
                        text(class = "tab__text") {"Logs"}
                    }
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Settings,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Settings),
                    on:click = tab_click(Tab::Settings)
                ) {
                    box(class = "tab__label", var:--tab-label-w = Some("66px".to_owned())) {
                        text(class = "tab__text") {"Settings"}
                    }
                }
                // A tab that changes nothing is worse than a tab that says it
                // cannot be used yet, so it is drawn and never selected.
                box(class = "tab tab--off", attr:aria-disabled = "true") {
                    text {"Watch"}
                }
                spacer()
                // The filter lives in the tab strip: a field of its own would
                // cost a whole row for one control. It is only in the strip on
                // Channels, which is the only tab it filters.
                if move || tab.get() == Tab::Channels {
                    Input(
                        value = channel_filter,
                        placeholder = "Filter channels",
                        class = "tabs__filter",
                        on:key_down = move |ev| {
                            if matches!(ev.key, Key::Named(NamedKey::Escape)) {
                                channel_filter.set(String::new());
                                ev.prevent_default();
                            }
                        },
                    )
                }
                text(class = "conn") {
                    {move || conn_summary(conn_sig.get(), &channels_sig.get())}
                }
            }

            if move || tab.get() == Tab::Channels {
                ChannelsTab(
                    channels = channels_sig,
                    pins = pins_sig,
                    pipelines = pipelines_sig,
                    now = now,
                    pending = pending_sig,
                    expanded = expanded,
                    closing = closing,
                    draft = draft,
                    intents = Rc::clone(&channels_intents),
                    toggle_pin = toggle_pin,
                    filter = channel_filter,
                    settings = settings_sig,
                    set_prefs = UnsyncCallback::new({
                        let state = state_channels.clone();
                        move |(id, live, titles): (u64, bool, bool)| {
                            state.set_channel_prefs(id, |prefs| {
                                prefs.notify_live = live;
                                prefs.notify_titles = titles;
                            });
                        }
                    }),
                )
            }
            if move || tab.get() == Tab::Filter {
                FilterTab(
                    settings = settings_sig,
                    draft = word_draft,
                    state = state_filter.clone(),
                )
            }
            if move || tab.get() == Tab::Recordings {
                RecordingsTab(
                    pipelines = pipelines_sig,
                    channels = channels_sig,
                    draft = once_login,
                    quality = once_quality,
                    intents = Rc::clone(&recordings_intents),
                )
            }
            if move || tab.get() == Tab::Logs {
                LogsTab(
                    conn = conn_sig,
                    channels = channels_sig,
                    arena = arena_sig,
                    downloader = downloader_sig,
                    notices = notices_sig,
                    logs = logs_sig,
                    pipelines = pipelines_sig,
                )
            }
            if move || tab.get() == Tab::Settings {
                SettingsTab(
                    settings = settings_sig,
                    display = Rc::clone(&display),
                )
            }
        }
    }
}

/// The scrollbar's visible thumb.
///
/// The engine paints its own scrollbar with the scrollport, before the rows, so
/// the rows' gutter strips cover it. This draws the same truth after them: it
/// reads the scroll position the [`ScrollArea`] publishes (the engine's own
/// source) and takes no pointer events, so dragging and paging still reach the
/// engine's track underneath, while the engine's own thumb is made transparent
/// in the sheet.
#[component]
fn ListThumb() -> impl IntoView {
    let context = ScrollAreaContext::current();
    let element = NodeRef::new();
    let metrics = Signal::derive_local(move || {
        let position = context?.position();
        let scale = element.scale();
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let extent = position.content_size.height.0 / scale;
        let viewport = position.scrollport.height.0 / scale;
        let offset = position.offset.y.0 / scale;
        (extent > viewport + 0.5).then(|| {
            let height = (viewport * viewport / extent).max(24.0);
            let top = offset / (extent - viewport) * (viewport - height);
            (top, height)
        })
    });
    view! {
        box(
            node_ref = element,
            class = "list-thumb",
            class:hidden = move || metrics.get().is_none(),
            a11y:hidden = true,
            style:top = move || metrics.get().map(|(top, _)| format!("{top:.1}px")),
            style:height = move || metrics.get().map(|(_, height)| format!("{height:.1}px")),
        )
    }
}

#[component]
#[allow(
    clippy::needless_pass_by_value,
    clippy::redundant_clone,
    clippy::too_many_arguments,
    reason = "every argument is one reactive handle the view reads"
)]
fn ChannelsTab(
    channels: RwSignal<Rc<Vec<RwSignal<ChannelView, LocalStorage>>>, LocalStorage>,
    pins: RwSignal<Rc<Vec<u64>>, LocalStorage>,
    pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
    now: RwSignal<i64, LocalStorage>,
    pending: RwSignal<Rc<Vec<String>>, LocalStorage>,
    expanded: RwSignal<BTreeSet<u64>, LocalStorage>,
    closing: RwSignal<BTreeSet<u64>, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    filter: RwSignal<String, LocalStorage>,
    intents: Rc<IntentSender>,
    toggle_pin: UnsyncCallback<u64>,
    settings: RwSignal<ClientSettings, LocalStorage>,
    set_prefs: UnsyncCallback<(u64, bool, bool)>,
) -> impl IntoView {
    let add_login: Rc<dyn Fn()> = {
        let intents = Rc::clone(&intents);
        Rc::new(move || {
            let login = draft.get_untracked().trim().to_owned();
            if login.is_empty() {
                return;
            }
            intents.send(Request::AddLogin { login });
            draft.set(String::new());
        })
    };
    let rows = Rc::clone(&intents);
    // Twitch logins top out at 25 characters; stop the field there rather than
    // letting a longer string reach GQL and come back as "not found".
    let clamp_login = UnsyncCallback::new(move |value: String| {
        if value.chars().count() > 25 {
            draft.set(value.chars().take(25).collect());
        }
    });

    // A view of the same keyed list, not a second order: the filter only
    // decides which rows are drawn, so pins and the sort still hold underneath.
    // It matches the login and the display name; a title changes constantly and
    // is not a stable way to find a channel. The field itself is in the tab
    // strip above.
    view! {
        column(class = "tabbody") {
            row(class = "tablehead") {
                text(class = "cell cell--dot") {""}
                text(class = "cell cell--chevron") {""}
                text(class = "cell cell--channel") {"Channel"}
                text(class = "cell cell--title") {"Title"}
                box(class = "cell cell--notify notify-head") {
                    text(class = "notify-head__group") {"Notify"}
                    row(class = "notify-head__labels") {
                        text(class = "notify-head__label") {"live"}
                        text(class = "notify-head__label") {"title"}
                    }
                }
                text(class = "cell cell--record") {"Record"}
                text(class = "cell cell--viewers") {"Uptime"}
            }
            ScrollArea(class = "channels") {
                column(class = "list") {
                    for channel in move || {
                        let needle = filter.get().trim().to_lowercase();
                        (*channels.get())
                            .iter()
                            .filter(|row| row.with_untracked(|channel| matches_filter(channel, &needle)))
                            .copied()
                            .collect::<Vec<_>>()
                    }, key = |channel: &RwSignal<ChannelView, LocalStorage>| channel.get_untracked().id {
                        ChannelRow(
                            channel = channel,
                            pins = pins,
                            pipelines = pipelines,
                            now = now,
                            expanded = expanded,
                            closing = closing,
                            intents = Rc::clone(&rows),
                            toggle_pin = toggle_pin,
                            settings = settings,
                            set_prefs = set_prefs,
                        )
                    }
                    for login in move || {
                        let needle = filter.get().trim().to_lowercase();
                        (*pending.get())
                            .iter()
                            .filter(|login| needle.is_empty() || login.to_lowercase().contains(&needle))
                            .cloned()
                            .collect::<Vec<_>>()
                    }, key = |login: &String| login.clone() {
                        row(class = "channel-row pending") {
                            text(class = "cell cell--dot") {""}
                            box(class = "live-dot") {box(class = "live-dot__lamp")}
                            text(class = "cell cell--channel") {{login.clone()}}
                            text(class = "cell cell--title") {"resolving…"}
                        }
                    }
                    if move || channels.get().is_empty() && pending.get().is_empty() {
                        text(class = "empty") {
                            "No channels yet. Add a Twitch login below to follow a channel."
                        }
                    }
                    if move || {
                        let needle = filter.get().trim().to_lowercase();
                        if needle.is_empty() || (channels.get().is_empty() && pending.get().is_empty()) {
                            return false;
                        }
                        let matched = channels.get().iter().any(|row| {
                            row.with_untracked(|channel| matches_filter(channel, &needle))
                        });
                        !matched
                            && !pending.get().iter().any(|login| {
                                login.to_lowercase().contains(&needle)
                            })
                    } {
                        text(class = "empty") {
                            {move || format!("No channel matches “{}”", filter.get().trim())}
                        }
                    }
                }
                ListThumb()
            }
            row(class = "add") {
                Input(
                    value = draft,
                    placeholder = "Add a new streamer",
                    on_change = clamp_login,
                    class = "add__input",
                    on:key_down = {
                        let add_login = Rc::clone(&add_login);
                        move |ev| on_enter(ev, || add_login())
                    },
                )
                Button(on:click = move |_| add_login()) {"Add"}
            }
        }
    }
}

#[component]
#[allow(
    clippy::needless_pass_by_value,
    clippy::redundant_clone,
    clippy::too_many_arguments,
    reason = "every argument is one reactive handle the row reads"
)]
fn ChannelRow(
    channel: RwSignal<ChannelView, LocalStorage>,
    pins: RwSignal<Rc<Vec<u64>>, LocalStorage>,
    pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
    now: RwSignal<i64, LocalStorage>,
    expanded: RwSignal<BTreeSet<u64>, LocalStorage>,
    closing: RwSignal<BTreeSet<u64>, LocalStorage>,
    intents: Rc<IntentSender>,
    toggle_pin: UnsyncCallback<u64>,
    settings: RwSignal<ClientSettings, LocalStorage>,
    set_prefs: UnsyncCallback<(u64, bool, bool)>,
) -> impl IntoView {
    // Nothing below reads `channel` directly: the component body runs once, so
    // every value that has to follow the daemon is a derived signal read by the
    // hole that draws it.
    let id = channel.get_untracked().id;
    // A panel that is on its way out is still drawn, and the row keeps its
    // open styling until it has gone.
    let open =
        Signal::derive_local(move || expanded.get().contains(&id) || closing.get().contains(&id));
    let closing_this = Signal::derive_local(move || closing.get().contains(&id));
    // The pending exit timeout, so re-opening cancels it instead of letting it
    // unmount a panel the user just asked for.
    let close_timer: Rc<RefCell<Option<zgui::view::TimeoutHandle>>> = Rc::new(RefCell::new(None));
    let display_name = Signal::derive_local(move || channel.with(|c| c.display_name.clone()));
    let initials =
        Signal::derive_local(move || initials(&channel.with(|c| c.display_name.clone())));
    let avatar_path = Signal::derive_local(move || channel.with(|c| c.avatar_path.clone()));
    let color = Signal::derive_local(move || channel.with(|c| c.chat_color));
    let live = Signal::derive_local(move || channel.with(|c| c.live));
    let title = Signal::derive_local(move || {
        channel
            .with(|c| c.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "—".to_owned())
    });
    // Only titles that cannot fit their cell scroll. Three copies make the run
    // periodic over a third of its own width, which is what lets the offset
    // wrap with no seam and no measurement: `translateX(-33.333%)` lands
    // exactly on the next copy whatever the glyphs are.
    let title_run = Signal::derive_local(move || {
        let title = title.get();
        if title.chars().count() > TITLE_SCROLL_MIN_CHARS {
            format!("{title}{TITLE_SEPARATOR}").repeat(3)
        } else {
            title
        }
    });
    // One copy's length in characters, which is the pace's unit: a copy per
    // `copy_chars / TITLE_SCROLL_CHARS_PER_SEC` seconds, so a long title takes
    // longer to go round instead of racing.
    let title_copy_chars = Signal::derive_local(move || {
        let count = u16::try_from(title.get().chars().count()).unwrap_or(u16::MAX);
        if usize::from(count) > TITLE_SCROLL_MIN_CHARS {
            f32::from(count) + TITLE_SEPARATOR_CHARS as f32
        } else {
            0.0
        }
    });
    // A fraction of one copy, not a pixel offset: the wrap is where the run is
    // identical to itself, so the loop cannot drift against the text's width.
    let title_progress = RwSignal::new_local(0.0f32);
    let title_gen = Rc::new(Cell::new(0u64));
    let timers = Timers::current();
    let enter = {
        let token = Rc::clone(&title_gen);
        let timers = timers.clone();
        move |_: &mut EventCx<'_, events::PointerEnter>| {
            if let Some(timers) = timers.as_ref() {
                scroll_title(
                    timers,
                    title_progress,
                    title_copy_chars.get_untracked(),
                    Rc::clone(&token),
                    false,
                );
            }
        }
    };
    let leave = {
        let token = Rc::clone(&title_gen);
        let timers = timers.clone();
        move |_: &mut EventCx<'_, events::PointerLeave>| {
            if let Some(timers) = timers.as_ref() {
                scroll_title(
                    timers,
                    title_progress,
                    title_copy_chars.get_untracked(),
                    Rc::clone(&token),
                    true,
                );
            }
        }
    };

    let pinned = Signal::derive_local(move || pins.get().contains(&id));
    // The pipeline key is the channel id and a quality, so a row claims its
    // recording by id.
    let recording = Signal::derive_local(move || {
        pipelines.get().iter().any(|pipeline| {
            pipeline.with(|pipeline| pipeline.key.channel == id && is_recorded(pipeline))
        })
    });
    let error = Signal::derive_local(move || channel.with(|c| channel_error(c, &pipelines.get())));
    // Live rows show how long the stream has been running; offline rows show
    // when they were last live, which is the only useful thing a dead channel
    // has to say. One right-aligned column, and the dot already carries the
    // live/offline state.
    let stat_label = Signal::derive_local(move || {
        channel.with(|c| {
            if c.live {
                return c
                    .stream_start_ms
                    .map_or_else(|| "—".to_owned(), |start| format_uptime(now.get() - start));
            }
            c.last_live_ms
                .map_or_else(|| "—".to_owned(), |at| format_ago(at, now.get()))
        })
    });
    let stat_tip = Signal::derive_local(move || {
        channel.with(|c| {
            if c.live {
                return c.stream_start_ms.map_or_else(
                    || "Live".to_owned(),
                    |start| format!("Live for {}", format_uptime(now.get() - start)),
                );
            }
            c.last_live_ms.map_or_else(
                || "Has not been live yet".to_owned(),
                |at| format!("Last live {}", format_iso_ms(at)),
            )
        })
    });
    // In the component body, not in the click: the queue is reached through
    // the scope tree, and a handler runs outside it.
    let toasts = use_toaster();

    let record_intents = Rc::clone(&intents);
    let detail_intents = Rc::clone(&intents);

    // The notify switches are the client's own, so they are written straight to
    // the settings document rather than sent to the daemon. The record switch
    // stays an intent: it is the daemon's word to give.
    let prefs = Signal::derive_local(move || settings.with(|settings| settings.channel(id)));
    let notify_live = Binding::controlled(
        Signal::derive_local(move || prefs.get().notify_live),
        move |on: bool| {
            set_prefs.run((id, on, prefs.get_untracked().notify_titles));
        },
    );
    let notify_titles = Binding::controlled(
        Signal::derive_local(move || prefs.get().notify_titles),
        move |on: bool| {
            set_prefs.run((id, prefs.get_untracked().notify_live, on));
        },
    );
    let record = Binding::controlled(
        Signal::derive_local(move || channel.with(|c| c.record)),
        move |on: bool| {
            record_intents.send(Request::SetRecord {
                channel: id,
                record: on,
            });
        },
    );

    let pin_select = UnsyncCallback::new(move |()| toggle_pin.run(id));
    let remove_select = UnsyncCallback::new({
        let intents = Rc::clone(&intents);
        move |()| {
            remove_channel(Rc::clone(&intents), &channel.get_untracked(), toasts);
        }
    });

    view! {
        ContextMenu {
            ContextMenuTrigger(class = "channel") {
                row(
                    class = "channel-row",
                    class:open = open,
                    class:offline = move || !live.get(),
                    // The row, the switches and the chevron read this; the
                    // panel sets it again on its own box, because a component
                    // boundary is not a place to rely on inheritance.
                    var:--channel-color = move || Some(color.get().to_string()),
                    on:pointer_enter = enter,
                    on:pointer_leave = leave,
                    on:click = {
                        let close_timer = Rc::clone(&close_timer);
                        move |_| {
                            if closing.get_untracked().contains(&id) {
                                // Re-opened while rolling away: keep it mounted
                                // and cancel the unmount.
                                closing.update(|set| {
                                    set.remove(&id);
                                });
                                *close_timer.borrow_mut() = None;
                            } else if expanded.get_untracked().contains(&id) {
                                closing.update(|set| {
                                    set.insert(id);
                                });
                                *close_timer.borrow_mut() =
                                    Some(set_timeout(
                                        Duration::from_millis(DETAIL_ROLL_MS),
                                        move || {
                                            closing.update(|set| {
                                                set.remove(&id);
                                            });
                                            expanded.update(|set| {
                                                set.remove(&id);
                                            });
                                        },
                                    ));
                            } else {
                                expanded.update(|set| {
                                    set.insert(id);
                                });
                            }
                        }
                    }
                ) {
                    box(class = "live-dot", class:live = live) {box(class = "live-dot__lamp")}
                    box(class = "chevron", class:open = open) {
                        box(class = "chevron__tip")
                    }
                    Tooltip {
                        TooltipTrigger(class = "cell cell--channel") {
                            row(class = "identity") {
                                Avatar(
                                    size = AvatarSize::Sm,
                                    src = avatar_path,
                                    label = Some(display_name.get_untracked()),
                                ) {
                                    text {{move || initials.get()}}
                                }
                                text(class = "channel-name") {{move || display_name.get()}}
                            }
                        }
                        TooltipContent {{format!("Channel ID {id}")}}
                    }
                    Tooltip {
                        TooltipTrigger(class = "cell cell--title") {
                            row(class = "title-cell") {
                                // The row's state marks lead the title: a
                                // failure, a recording, and a pin all describe
                                // the row as a whole, and none of them is worth
                                // a column of its own.
                                if move || error.get().is_some() {
                                    box(class = "mark mark--error") {
                                        text(class = "mark__glyph") {"!"}
                                    }
                                }
                                if move || recording.get() {
                                    box(class = "rec-pill") {text {"REC"}}
                                }
                                if move || pinned.get() {
                                    box(class = "pin") {
                                        box(class = "pin__head")
                                        box(class = "pin__stem")
                                    }
                                }
                                box(class = "channel-title") {
                                    text(
                                        class = "channel-title__run",
                                        // Inline, not a custom property: this
                                        // zgui does not apply `var()` inside
                                        // `translateX()`.
                                        style:transform = move || {
                                            // One copy is a third of the run's
                                            // width, and `%` is the run's own
                                            // width, so the wrap is exact
                                            // whatever the glyphs are.
                                            Some(format!(
                                                "translateX(-{:.3}%)",
                                                title_progress.get() * (100.0 / 3.0)
                                            ))
                                        },
                                    ) {{move || title_run.get()}}
                                }
                            }
                        }
                        TooltipContent {{move || error.get().unwrap_or_else(|| stream_tooltip(&channel.get()))}}
                    }
                    box(class = "cell cell--notify", on:click:stop = |_| {}) {
                        Tooltip {
                            TooltipTrigger {
                                Switch(
                                    size = SwitchSize::Sm,
                                    checked = notify_live,
                                )
                            }
                            TooltipContent {"Notify when this channel goes live"}
                        }
                        Tooltip {
                            TooltipTrigger {
                                Switch(
                                    size = SwitchSize::Sm,
                                    checked = notify_titles,
                                )
                            }
                            TooltipContent {"Notify when the title or game changes"}
                        }
                    }
                    box(class = "cell cell--record", on:click:stop = |_| {}) {
                        Switch(
                            size = SwitchSize::Sm,
                            checked = record,
                        )
                    }
                    Tooltip {
                        TooltipTrigger(class = "cell cell--viewers viewers-cell") {
                            text(
                                class = "viewers__value",
                                class:stale = move || !live.get(),
                            ) {{move || stat_label.get()}}
                        }
                        TooltipContent {{move || stat_tip.get()}}
                    }
                }
                if move || open.get() {
                    box(
                        class = "detail-clip",
                        class:closing = closing_this
                    ) {
                        {channel_detail(channel, &detail_intents, pins, toggle_pin, toasts)}
                    }
                }
            }
            // The menu is the accelerator; every one of these actions is also
            // in the expanded panel. A row whose only way to do something is a
            // right-click is a row a keyboard cannot use.
            ContextMenuContent {
                if move || pinned.get() {
                    MenuItem(on_select = pin_select) {"Unpin"}
                } else {
                    MenuItem(on_select = pin_select) {"Pin to top"}
                }
                MenuSeparator()
                MenuItem(destructive = true, on_select = remove_select) {
                    "Remove channel"
                }
            }
        }
    }
}

/// Removes a channel and offers the undo that makes it safe.
///
/// The daemon's removal is immediate (its monitors and topics go with it), so
/// the undo is a real re-add. The per-channel switches are the client's and are
/// deliberately not pruned on removal, so the re-add finds them intact.
fn remove_channel(intents: Rc<IntentSender>, view: &ChannelView, toasts: Option<ToastQueue>) {
    let id = view.id;
    let name = if view.display_name.is_empty() {
        view.login.clone()
    } else {
        view.display_name.clone()
    };
    intents.send(Request::RemoveChannel(id));
    let Some(toasts) = toasts else {
        return;
    };
    let login = view.login.clone();
    toasts.push(
        Toast::new(format!("Removed {name}"))
            .duration(Duration::from_secs(8))
            .action("Undo", move || {
                intents.send(Request::AddLogin {
                    login: login.clone(),
                });
            }),
    );
}

/// The expanded body, with its own handle on the intents.
///
/// The enclosing reactive `if` is an `Fn` that may be evaluated more than once,
/// so it can only lend the row's clone; this builds a body that owns one.
fn channel_detail(
    channel: RwSignal<ChannelView, LocalStorage>,
    intents: &Rc<IntentSender>,
    pins: RwSignal<Rc<Vec<u64>>, LocalStorage>,
    toggle_pin: UnsyncCallback<u64>,
    toasts: Option<ToastQueue>,
) -> impl IntoView {
    let intents = Rc::clone(intents);
    move || channel_detail_view(channel.get(), intents.clone(), pins, toggle_pin, toasts)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the returned view borrows the reference it is given"
)]
fn channel_detail_view(
    channel: ChannelView,
    intents: Rc<IntentSender>,
    pins: RwSignal<Rc<Vec<u64>>, LocalStorage>,
    toggle_pin: UnsyncCallback<u64>,
    toasts: Option<ToastQueue>,
) -> impl IntoView {
    let id = channel.id;
    let pinned = Signal::derive_local(move || pins.get().contains(&id));
    let login = channel.login.clone();
    let title = channel
        .title
        .clone()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "No title".to_owned());
    let stream_id = channel
        .stream_id
        .map_or_else(|| "—".to_owned(), |id| id.to_string());
    let game_id = channel
        .game_id
        .map_or_else(|| "—".to_owned(), |id| id.to_string());
    let game = channel
        .game
        .clone()
        .filter(|game| !game.is_empty())
        .unwrap_or_else(|| "no game".to_owned());
    let game_tip = channel.game_id.map_or_else(
        || "No game id resolved".to_owned(),
        |id| format!("Game ID {id}"),
    );
    let collab = channel
        .collaboration_viewers
        .map_or_else(|| "—".to_owned(), format_count);
    let started = channel
        .stream_start_ms
        .map_or_else(|| "—".to_owned(), format_iso_ms);
    let last_live = channel
        .last_live_ms
        .map_or_else(|| "—".to_owned(), format_iso_ms);
    let viewers = channel.viewers.map_or_else(|| "—".to_owned(), format_count);
    let error = channel.error.clone();
    let error_line = error.clone();
    let remove_view = channel.clone();
    let remove_intents = Rc::clone(&intents);
    let channel_color = channel.chat_color.to_string();

    view! {
        column(class = "detail", var:--channel-color = Some(channel_color)) {
            row(class = "detail-head") {
                text(class = "detail-login") {{format!("@{login}")}}
            }
            text(class = "detail-title") {{title}}
            if move || error.is_some() {
                text(class = "detail-error") {{error_line.clone().unwrap_or_default()}}
            }
            row(class = "detail-grid") {
                column(class = "detail-col") {
                    {detail_item("Channel ID", channel.id.to_string())}
                    {detail_item("Stream ID", stream_id)}
                    {detail_item("Game ID", game_id)}
                    {detail_item_tip("Game", game, game_tip)}
                }
                column(class = "detail-col") {
                    {detail_item("Viewers", viewers)}
                    {detail_item("Collab", collab)}
                    {detail_item("Started", started)}
                    {detail_item("Last live", last_live)}
                }
            }
            row(class = "detail-foot") {
                Button(
                    size = ButtonSize::Sm,
                    variant = ButtonVariant::Ghost,
                    on:click = move |_| toggle_pin.run(id)
                ) {
                    {move || if pinned.get() { "Unpin" } else { "Pin to top" }}
                }
                spacer()
                Button(
                    size = ButtonSize::Sm,
                    variant = ButtonVariant::Outline,
                    class = "detail-remove",
                    on:click = move |_| {
                        remove_channel(Rc::clone(&remove_intents), &remove_view, toasts);
                    }
                ) {"Remove"}
            }
        }
    }
}

fn detail_item(key: &'static str, value: String) -> impl IntoView {
    view! {
        row(class = "detail-item") {
            text(class = "detail-key") {{key}}
            text(class = "detail-value") {{value}}
        }
    }
}

fn detail_item_tip(key: &'static str, value: String, tip: String) -> impl IntoView {
    view! {
        row(class = "detail-item") {
            text(class = "detail-key") {{key}}
            Tooltip {
                TooltipTrigger(class = "detail-value") {
                    text(class = "detail-value__text") {{value}}
                }
                TooltipContent {{tip.clone()}}
            }
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn FilterTab(
    settings: RwSignal<ClientSettings, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    state: UiState,
) -> impl IntoView {
    let add_word: Rc<dyn Fn()> = {
        let state = state.clone();
        Rc::new(move || {
            let word = draft.get_untracked().trim().to_owned();
            if word.is_empty() {
                return;
            }
            state.add_word(&word);
            draft.set(String::new());
        })
    };

    view! {
        column(class = "tabbody") {
            text(class = "section-title") {"Filtered words"}
            text(class = "section-hint") {"A notification whose title contains one of these is dropped."}
            row(class = "add") {
                Input(
                    value = draft,
                    placeholder = "Add a filtered word",
                    class = "add__input",
                    on:key_down = {
                        let add_word = Rc::clone(&add_word);
                        move |ev| on_enter(ev, || add_word())
                    },
                )
                Button(on:click = move |_| add_word()) {"Add"}
            }
            ScrollArea(class = "words") {
                column(class = "list") {
                    for word in move || settings.with(|settings| settings.notifications.filtered_words.clone()), key = |word: &String| word.clone() {
                        WordRowView(word = word, state = state.clone())
                    }
                }
            }
        }
    }
}

/// Everything that is a preference rather than a list.
///
/// Two sections, both the client's: the screen the window is on, and what this
/// window does with a stream transition.
#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn SettingsTab(
    settings: RwSignal<ClientSettings, LocalStorage>,
    display: Rc<Display>,
) -> impl IntoView {
    view! {
        column(class = "tabbody") {
            text(class = "section-title") {"Display"}
            column(class = "settings") {
                row(class = "switch-row") {
                    column(class = "switch-copy") {
                        text(class = "switch-title") {"High dynamic range"}
                        text(class = "switch-hint") {"Present in PQ/Rec.2020 when the display is already in HDR mode. Windows only."}
                    }
                    spacer()
                    Switch(
                        checked = Binding::controlled(
                            Signal::derive_local({
                                let display = Rc::clone(&display);
                                move || display.get().hdr != HdrMode::Off
                            }),
                            {
                                let display = Rc::clone(&display);
                                move |on: bool| {
                                    let current = display.present();
                                    display.set(
                                        if on { HdrMode::Auto } else { HdrMode::Off },
                                        current.tearing,
                                    );
                                }
                            },
                        )
                    )
                }
                row(class = "switch-row") {
                    column(class = "switch-copy") {
                        text(class = "switch-title") {"Variable refresh"}
                        text(class = "switch-hint") {"Show the newest frame rather than queueing them, and let the display follow the frame rate. May tear on a fixed-rate display. Windows only."}
                    }
                    spacer()
                    Switch(
                        checked = Binding::controlled(
                            Signal::derive_local({
                                let display = Rc::clone(&display);
                                move || display.get().tearing
                            }),
                            {
                                let display = Rc::clone(&display);
                                move |on: bool| {
                                    let current = display.present();
                                    display.set(current.hdr, on);
                                }
                            },
                        )
                    )
                }
            }
            text(class = "section-title") {"Notifications"}
            column(class = "settings") {
                row(class = "switch-row") {
                    column(class = "switch-copy") {
                        text(class = "switch-title") {"Title and game changes"}
                        text(class = "switch-hint") {"Notify when a followed channel changes its title or game."}
                    }
                    spacer()
                    Switch(
                        checked = Binding::controlled(
                            Signal::derive_local(move || settings.with(|settings| settings.notifications.title_changes)),
                            move |on| {
                                crate::settings::update(settings, |settings| {
                                    settings.notifications.title_changes = on;
                                });
                            },
                        )
                    )
                }
                row(class = "switch-row") {
                    column(class = "switch-copy") {
                        text(class = "switch-title") {"Notification sound"}
                        text(class = "switch-hint") {"Play a sound with every toast."}
                    }
                    spacer()
                    Switch(
                        checked = Binding::controlled(
                            Signal::derive_local(move || settings.with(|settings| settings.notifications.sound)),
                            move |on| {
                                crate::settings::update(settings, |settings| {
                                    settings.notifications.sound = on;
                                });
                            },
                        )
                    )
                }
            }
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn WordRowView(word: String, state: UiState) -> impl IntoView {
    let remove = word.clone();
    view! {
        row(class = "word") {
            text(class = "word__text") {{word}}
            spacer()
            Button(
                size = ButtonSize::IconSm,
                variant = ButtonVariant::Ghost,
                on:click = move |_| {
                    state.remove_word(&remove);
                }
            ) {"×"}
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn RecordingsTab(
    pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
    channels: RwSignal<Rc<Vec<RwSignal<ChannelView, LocalStorage>>>, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    quality: RwSignal<String, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let record_once: Rc<dyn Fn()> = {
        let intents = Rc::clone(&intents);
        Rc::new(move || {
            let login = draft.get_untracked().trim().to_lowercase();
            if login.is_empty() {
                return;
            }
            let quality = quality.get_untracked().trim().to_owned();
            intents.send(Request::RecordOnce {
                login,
                quality: (!quality.is_empty()).then_some(quality),
            });
            draft.set(String::new());
        })
    };
    let rows = Rc::clone(&intents);

    view! {
        column(class = "tabbody") {
            row(class = "tablehead") {
                text(class = "cell cell--channel") {"Channel"}
                text(class = "cell cell--quality") {"Quality"}
                text(class = "cell cell--state") {"State"}
                text(class = "cell cell--position") {"Position"}
                text(class = "cell cell--segments") {"Segments"}
                text(class = "cell cell--size") {"Size"}
                text(class = "cell cell--runs") {"Runs"}
                text(class = "cell cell--action") {""}
            }
            ScrollArea(class = "recordings") {
                column(class = "list") {
                    for pipeline in move || recordings(&pipelines.get()), key = |pipeline: &RwSignal<PipelineView, LocalStorage>| {
                        let key = pipeline.get_untracked().key;
                        (key.channel, key.quality)
                    } {
                        RecordingRow(
                            pipeline = pipeline,
                            channels = channels,
                            intents = Rc::clone(&rows),
                        )
                    }
                    if move || recordings(&pipelines.get()).is_empty() {
                        text(class = "empty") {"No recording is running. Pick a channel above and press Record once, or turn Record on in Channels."}
                    }
                }
            }
            row(class = "add") {
                Input(
                    value = draft,
                    placeholder = "Add a channel to record once",
                    class = "add__input",
                    on:key_down = {
                        let record_once = Rc::clone(&record_once);
                        move |ev| on_enter(ev, || record_once())
                    },
                )
                Input(
                    value = quality,
                    placeholder = "quality (optional)",
                    class = "add__quality",
                    on:key_down = {
                        let record_once = Rc::clone(&record_once);
                        move |ev| on_enter(ev, || record_once())
                    },
                )
                Button(on:click = move |_| record_once()) {"Record once"}
            }
        }
    }
}

const fn is_recorded(pipeline: &PipelineView) -> bool {
    pipeline.recording || pipeline.disk.is_some()
}

/// The rows the Recordings tab draws. Membership follows the row's own signal,
/// so `UiState` re-runs the list whenever a pipeline starts or stops recording.
/// The login of a channel, for display: pipelines are keyed by channel id.
fn login_for(
    channels: RwSignal<Rc<Vec<RwSignal<ChannelView, LocalStorage>>>, LocalStorage>,
    channel: u64,
) -> String {
    channels
        .get()
        .iter()
        .find(|row| row.get().id == channel)
        .map_or_else(String::default, |row| row.get().login)
}

fn recordings(
    pipelines: &[RwSignal<PipelineView, LocalStorage>],
) -> Vec<RwSignal<PipelineView, LocalStorage>> {
    pipelines
        .iter()
        .filter(|pipeline| pipeline.with_untracked(is_recorded))
        .copied()
        .collect()
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn RecordingRow(
    pipeline: RwSignal<PipelineView, LocalStorage>,
    channels: RwSignal<Rc<Vec<RwSignal<ChannelView, LocalStorage>>>, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let key = pipeline.get_untracked().key;
    let quality = key.quality.clone().unwrap_or_else(|| "best".to_owned());
    let login = Signal::derive_local(move || {
        login_for(channels, pipeline.with(|pipeline| pipeline.key.channel))
    });
    let state = Signal::derive_local(move || state_label(pipeline.get().state));
    let position = Signal::derive_local(move || {
        pipeline.with(|pipeline| {
            pipeline
                .disk
                .as_ref()
                .map_or_else(String::new, |disk| format_duration_ms(disk.position_ms))
        })
    });
    let segments = Signal::derive_local(move || {
        pipeline.with(|pipeline| {
            pipeline
                .disk
                .as_ref()
                .map_or_else(String::new, |disk| disk.segments.to_string())
        })
    });
    let size = Signal::derive_local(move || {
        pipeline.with(|pipeline| {
            pipeline
                .disk
                .as_ref()
                .map_or_else(String::new, |disk| format_bytes(disk.bytes))
        })
    });
    let runs = Signal::derive_local(move || {
        pipeline.with(|pipeline| {
            pipeline
                .disk
                .as_ref()
                .map_or_else(String::new, |disk| disk.runs.to_string())
        })
    });
    let error = Signal::derive_local(move || {
        pipeline.with(|pipeline| {
            pipeline
                .disk
                .as_ref()
                .and_then(|disk| disk.last_error.clone())
                .or_else(|| pipeline.last_error.clone())
                .unwrap_or_default()
        })
    });
    let recording = Signal::derive_local(move || pipeline.with(|pipeline| pipeline.recording));
    let has_error = Signal::derive_local(move || !error.get().is_empty());

    let stop_intents = Rc::clone(&intents);
    view! {
        column(class = "recording") {
            row(class = "recording-row") {
                text(class = "cell cell--channel") {{move || login.get()}}
                text(class = "cell cell--quality") {{quality}}
                text(class = "cell cell--state") {{move || state.get()}}
                text(class = "cell cell--position") {{move || position.get()}}
                text(class = "cell cell--segments") {{move || segments.get()}}
                text(class = "cell cell--size") {{move || size.get()}}
                text(class = "cell cell--runs") {{move || runs.get()}}
                if move || recording.get() {
                    Button(
                        size = ButtonSize::Sm,
                        variant = ButtonVariant::Ghost,
                        on:click = {
                            let intents = Rc::clone(&stop_intents);
                            let key = key.clone();
                            move |_| {
                                intents.send(Request::PipelineStop {
                                    key: key.clone(),
                                    force: false,
                                });
                            }
                        }
                    ) {"Stop"}
                }
            }
            if move || has_error.get() {
                text(class = "recording__error") {{move || error.get()}}
            }
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn LogsTab(
    conn: RwSignal<ConnHealth, LocalStorage>,
    channels: RwSignal<Rc<Vec<RwSignal<ChannelView, LocalStorage>>>, LocalStorage>,
    arena: RwSignal<ArenaSummary, LocalStorage>,
    downloader: RwSignal<DownloaderSummary, LocalStorage>,
    notices: RwSignal<Rc<Vec<crate::riffle::Notice>>, LocalStorage>,
    logs: RwSignal<Rc<Vec<LogEntry>>, LocalStorage>,
    pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
) -> impl IntoView {
    view! {
        column(class = "tabbody") {
            row(class = "stats") {
                column(class = "stat") {
                    text(class = "stat__label") {"Connection"}
                    text(class = "stat__value") {{move || conn_summary(conn.get(), &channels.get())}}
                }
                column(class = "stat") {
                    text(class = "stat__label") {"Pipelines"}
                    text(class = "stat__value") {{move || pipelines.get().len().to_string()}}
                }
                column(class = "stat") {
                    text(class = "stat__label") {"Arena slots"}
                    text(class = "stat__value") {{move || format!("{} / {}", arena.get().live_slots, arena.get().high_water)}}
                }
                column(class = "stat") {
                    text(class = "stat__label") {"Downloader"}
                    text(class = "stat__value") {{move || format!("{} / {}", downloader.get().in_flight, downloader.get().capacity)}}
                }
            }
            ScrollArea(class = "logs") {
                column(class = "loglist") {
                    if move || !(*notices.get()).is_empty() {
                        text(class = "section-title") {"Notifications"}
                        for note in move || (*notices.get()).clone().into_iter().rev().collect::<Vec<_>>(), key = |note: &crate::riffle::Notice| note.id {
                            row(class = "notification") {
                                text(class = "log__time") {{format_time_ms(note.at_ms)}}
                                column(class = "notification__body") {
                                    text(class = "notification__summary") {{note.summary}}
                                    text(class = "notification__text") {{note.body}}
                                }
                            }
                        }
                    }
                    text(class = "section-title") {"Pipelines"}
                    if move || pipelines.get().is_empty() {
                        text(class = "empty") {"No pipeline is running."}
                    }
                    for pipeline in move || recordings(&pipelines.get()), key = |pipeline: &RwSignal<PipelineView, LocalStorage>| {
                        let key = pipeline.get_untracked().key;
                        (key.channel, key.quality)
                    } {
                        row(class = "pipeline") {
                            text(class = "pipeline__name") {{move || {
                                let key = pipeline.with(|pipeline| pipeline.key.clone());
                                let login = login_for(channels, key.channel);
                                format!("{}{}", login, key.quality.as_ref().map_or_else(String::new, |quality| format!(" · {quality}")))
                            }}}
                            text(class = "pipeline__state") {{move || state_label(pipeline.get().state)}}
                            text(class = "pipeline__meta") {{move || {
                                pipeline.with(|pipeline| format!("{} · {} viewers", format_duration_ms(pipeline.position_ms), pipeline.sessions))
                            }}}
                            text(class = "pipeline__rec", class:on = move || pipeline.get().recording) {"REC"}
                        }
                    }
                    text(class = "section-title") {"Events"}
                    for entry in move || (*logs.get()).clone().into_iter().rev().collect::<Vec<_>>(), key = |entry: &LogEntry| entry.seq {
                        {log_row(entry)}
                    }
                }
            }
        }
    }
}

fn log_row(entry: LogEntry) -> impl IntoView {
    let bad = entry.level == LogLevel::Error;
    let meta = [
        entry.login.as_ref().map(|login| format!("@{login}")),
        entry.stream_id.map(|id| format!("stream {id}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");

    view! {
        row(class = "log", class:bad = bad) {
            text(class = "log__time") {{format_time_ms(entry.at_ms)}}
            text(class = "log__target") {{entry.target}}
            text(class = "log__message") {{entry.message}}
            text(class = "log__meta") {{meta}}
        }
    }
}

const fn conn_label(conn: ConnHealth) -> &'static str {
    match conn {
        ConnHealth::Connected => "connected",
        ConnHealth::Connecting => "connecting…",
        ConnHealth::NoChannels => "no channels",
    }
}

/// Enter in a one-line field does what the field's button does.
///
/// A text input leaves `Enter` to its caller, so each add box has to opt in.
/// `Shift` is left alone: it is how a field that later grows into a composer
/// keeps its newline.
/// Whether a channel belongs in the list under the filter box: the login or the
/// display name contains `needle`, which is already lowercased. The title is
/// deliberately excluded — it changes constantly, so it is not what a person
/// types to find a channel again.
fn matches_filter(channel: &ChannelView, needle: &str) -> bool {
    needle.is_empty()
        || channel.login.to_lowercase().contains(needle)
        || channel.display_name.to_lowercase().contains(needle)
}

fn on_enter(ev: &EventCx<'_, events::KeyDown>, run: impl Fn()) {
    if matches!(ev.key, Key::Named(NamedKey::Enter)) && !ev.modifiers.shift() {
        run();
        ev.prevent_default();
    }
}

fn conn_summary(conn: ConnHealth, channels: &[RwSignal<ChannelView, LocalStorage>]) -> String {
    let live = channels
        .iter()
        .filter(|channel| channel.with_untracked(|channel| channel.live))
        .count();
    if live == 0 {
        conn_label(conn).to_owned()
    } else {
        format!("{} · {live} live", conn_label(conn))
    }
}

/// One loop of the top marquee: every channel's stream title, then the newest
/// notifications. Eight copies keep the strip filled even when one channel has
/// a short title and there are no notifications; the keyframe slides by one
/// copy (-12.5%) and lands on an identical string with no seam.
fn ticker_text(
    channels: &[RwSignal<ChannelView, LocalStorage>],
    notices: &[crate::riffle::Notice],
) -> String {
    let mut items = channels
        .iter()
        // Offline rows keep the title of a finished broadcast; a marquee full
        // of stale titles is worse than an empty strip, so only live ones run.
        .filter(|channel| channel.with_untracked(|channel| channel.live))
        .filter_map(|channel| {
            channel.with_untracked(|channel| {
                channel
                    .title
                    .as_deref()
                    .filter(|title| !title.is_empty())
                    .map(|title| format!("● {} — {}", channel.display_name, title))
            })
        })
        .collect::<Vec<_>>();
    items.extend(notices.iter().rev().take(8).map(|note| {
        if note.body.is_empty() {
            format!("● {}", note.summary)
        } else {
            format!("● {} — {}", note.summary, note.body)
        }
    }));
    if items.is_empty() {
        return "● no recent activity".to_owned();
    }
    let once = format!("{}     │     ", items.join("     │     "));
    once.repeat(8)
}

/// Seconds for one copy to scroll past. Roughly 30px/s at the ticker's 12px
/// type size, clamped so a bare notification list is not a blur and a full one
/// does not take a whole broadcast.
fn ticker_duration(text: &str) -> String {
    let per_copy = text.chars().count() / 8;
    let seconds = (per_copy as f32 * 0.22).clamp(18.0, 180.0);
    format!("{seconds:.0}s")
}

/// The weight axis for one tab during a switch: the incoming tab goes 600→800
/// while the outgoing one goes 800→600.
fn tab_weight_value(
    tab: RwSignal<Tab, LocalStorage>,
    previous: RwSignal<Option<Tab>, LocalStorage>,
    weight: RwSignal<f32, LocalStorage>,
    mine: Tab,
) -> impl Fn() -> Option<String> {
    move || {
        // Only the incoming and outgoing tabs take part in the tween;
        // everyone else stays light instead of blinking through the ramp.
        let progress = if tab.get() == mine {
            weight.get()
        } else if previous.get() == Some(mine) {
            1.0 - weight.get()
        } else {
            0.0
        };
        // Chillax's wght axis is 200–700; 200 is the lightest it can go.
        Some(format!("{:.0}", 400.0f32.mul_add(progress, 200.0)))
    }
}

/// Keeps a per-frame callback alive until `step` says it is done. The slot
/// holds the current frame handle; replacing it drops the one that just fired.
/// One frame of an animation, re-armed from its own body until `step` says it
/// is done.
///
/// The clock is a `Timers` taken in the component body, not the free
/// `request_frame`: a free request belongs to the scope that made it, and the
/// scope a pointer handler runs in goes away when the handler returns, which
/// stops the chain after its first frame. A `Timers` handle carries the
/// window's clock into the callback, so the animation survives the handler
/// that started it.
fn next_frame(
    timers: &Timers,
    generation: Rc<Cell<u64>>,
    token: u64,
    slot: &Rc<RefCell<Option<zgui::view::FrameHandle>>>,
    mut step: impl FnMut() -> bool + 'static,
) {
    let inner = Rc::clone(slot);
    let timers = timers.clone();
    let handle = timers.clone().request_frame(move |_| {
        if token != generation.get() {
            return;
        }
        if !step() {
            next_frame(&timers, Rc::clone(&generation), token, &inner, step);
        }
    });
    *slot.borrow_mut() = Some(handle);
}

/// Eases the variable weight from the old tab to the new one over 250ms. A
/// generation counter makes a quick series of tab clicks restart the tween
/// instead of letting older frames fight over the signal.
fn animate_tab_weight(
    timers: &Timers,
    weight: RwSignal<f32, LocalStorage>,
    generation: Rc<Cell<u64>>,
) {
    weight.set(0.0);
    let token = generation.get() + 1;
    generation.set(token);
    let started = std::time::Instant::now();
    let slot = Rc::new(RefCell::new(None));
    next_frame(timers, generation, token, &slot, move || {
        let t = (started.elapsed().as_secs_f32() / 0.25).min(1.0);
        // Cubic ease-out: the weight lands quickly, then settles into place.
        let eased = 1.0 - (1.0 - t).powi(3);
        weight.set(eased);
        t >= 1.0
    });
}

/// Drives a channel title's horizontal offset while its row is hovered, then
/// rewinds it to the start. Keeping the offset in a signal (rather than a CSS
/// animation) means leaving mid-scroll can smoothly return to zero.
/// A title that fits its cell does not scroll.
const TITLE_SCROLL_MIN_CHARS: usize = 55;
/// The gap between two copies of a scrolling title: five spaces, the bar, five
/// spaces. Counted in characters because the pace is.
const TITLE_SEPARATOR: &str = "     │     ";
const TITLE_SEPARATOR_CHARS: usize = 11;
/// Characters per second the hovered title moves. Expressed per character
/// rather than per pixel so a long title does not race: one copy takes
/// `copy_chars / TITLE_SCROLL_CHARS_PER_SEC` seconds, and the per-frame step
/// is scaled by the frame's own duration, so a 165 Hz window is not faster.
const TITLE_SCROLL_CHARS_PER_SEC: f32 = 8.5;
/// Rewind: an exponential pull home at this rate, plus this constant drag, both
/// in fractions of a copy per second.
const TITLE_REWIND_RATE: f32 = 8.0;
const TITLE_REWIND_PER_SEC: f32 = 0.6;
/// A frame this long is a stall (a covered window, a debugger break), not a
/// frame: clamping it keeps the title from jumping when the window comes back.
const TITLE_DT_MAX: f32 = 0.1;

/// How long the expanded panel takes to roll out and back in. The collapse
/// keeps it mounted this long so the exit animation can play before the view
/// removes it. Keep in step with the `detail-in`/`detail-out` keyframes.
const DETAIL_ROLL_MS: u64 = 200;

fn scroll_title(
    timers: &Timers,
    progress: RwSignal<f32, LocalStorage>,
    copy_chars: f32,
    generation: Rc<Cell<u64>>,
    rewind: bool,
) {
    if copy_chars <= 0.0 {
        return;
    }
    let token = generation.get() + 1;
    generation.set(token);
    let slot = Rc::new(RefCell::new(None));
    // Frame callbacks arrive at the display's rate, so every step is scaled by
    // the time since the last one rather than by a fixed per-frame amount.
    let mut last = std::time::Instant::now();
    next_frame(timers, generation, token, &slot, move || {
        let now_at = std::time::Instant::now();
        let dt = (now_at - last).as_secs_f32().min(TITLE_DT_MAX);
        last = now_at;

        let now = progress.get_untracked();
        let next = if rewind {
            // Pull home: `now - (now * rate + drag) * dt`, in copies per second.
            let pull = now.mul_add(TITLE_REWIND_RATE, TITLE_REWIND_PER_SEC);
            pull.mul_add(-dt, now).max(0.0)
        } else {
            // One copy per `copy_chars / rate` seconds, wrapping at the copy
            // boundary: the run is three identical copies, so zero shows the
            // same pixels as one and the loop cannot jump.
            let rate = TITLE_SCROLL_CHARS_PER_SEC / copy_chars;
            rate.mul_add(dt, now).fract()
        };
        progress.set(next);
        // Forward never finishes: the marquee loops for as long as the pointer
        // is on the row. Rewind stops when it is home.
        rewind && next <= 0.0
    });
}

const fn state_label(state: PipelineState) -> &'static str {
    match state {
        PipelineState::Starting => "starting",
        PipelineState::Live => "live",
        PipelineState::Offline => "offline",
        PipelineState::Stopped => "stopped",
        PipelineState::Failed => "failed",
    }
}

/// The first failure that belongs to a pipeline: the disk recorder's, the
/// stream's, or a scheduler that gave up.
fn pipeline_error(pipeline: &PipelineView) -> Option<String> {
    pipeline
        .disk
        .as_ref()
        .and_then(|disk| disk.last_error.clone())
        .or_else(|| pipeline.last_error.clone())
        .or_else(|| (pipeline.state == PipelineState::Failed).then(|| "pipeline failed".to_owned()))
}

/// Every failure the row knows about: the channel's own refresh and whatever
/// its pipelines say. The tab badge, the row mark and the detail all read this,
/// so they cannot disagree about whether something is wrong.
fn channel_error(
    channel: &ChannelView,
    pipelines: &[RwSignal<PipelineView, LocalStorage>],
) -> Option<String> {
    if let Some(error) = &channel.error {
        return Some(error.clone());
    }
    pipelines.iter().find_map(|pipeline| {
        pipeline.with(|pipeline| {
            if pipeline.key.channel == channel.id {
                pipeline_error(pipeline)
            } else {
                None
            }
        })
    })
}

fn stream_tooltip(channel: &ChannelView) -> String {
    channel.stream_id.map_or_else(
        || "No stream id resolved yet".to_owned(),
        |id| format!("Stream ID {id}"),
    )
}

fn initials(display_name: &str) -> String {
    let mut initials = display_name
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .take(2)
        .collect::<String>();
    if initials.is_empty() {
        initials = display_name.chars().take(2).collect();
    }
    initials.to_uppercase()
}
