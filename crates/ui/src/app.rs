//! The ZGUI view: header, tabs, channels, filters, recordings, logs, and the
//! renderer setup.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use siphon_shared::rpc::ArenaSummary;
use siphon_shared::rpc::ChannelSwitches;
use siphon_shared::rpc::ChannelView;
use siphon_shared::rpc::ConnHealth;
use siphon_shared::rpc::DownloaderSummary;
use siphon_shared::rpc::LogEntry;
use siphon_shared::rpc::LogLevel;
use siphon_shared::rpc::NotificationView;
use siphon_shared::rpc::PipelineState;
use siphon_shared::rpc::PipelineView;
use siphon_shared::rpc::Request;
use siphon_shared::settings::HdrMode;
use siphon_shared::settings::UiSettings;
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

use crate::format::format_ago;
use crate::format::format_bytes;
use crate::format::format_count;
use crate::format::format_duration_ms;
use crate::format::format_iso_ms;
use crate::format::format_time_ms;
use crate::format::format_uptime;
use crate::rpc::IntentSender;
use crate::rpc::UiEvent;
use crate::rpc::UiState;
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
    let state = UiState::new();
    state.set_pins(siphon_shared::settings::load_pins(crate::service::APP_ID).ids);
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

    // Frame timings for the resize A/B. stderr is what the app log captures.
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
    // The display settings are the client's own — they describe the screen the
    // window is on — so they are read here rather than from the daemon.
    let display = Display::load();
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

/// Registers the shipped fonts, then any dev overrides. Archivo and a CBDT
/// Noto Color Emoji are embedded so every platform renders the same text and
/// real colour emoji: the system emoji font can be COLRv1, which zgui's
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
    let notifications_sig = state.notifications;
    let logs_sig = state.logs;
    let words_sig = state.words;
    let title_sig = state.title_changes;
    let sound_sig = state.sound;

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
                // Whatever arrived alongside it lands in the same frame: a burst
                // costs one repaint rather than one per event.
                while let Ok(Some(event)) = events.try_recv() {
                    state.apply_event(event);
                }
            }
        }
    });

    let ticker =
        Signal::derive_local(move || ticker_text(&channels_sig.get(), &notifications_sig.get()));
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
    let once_login = RwSignal::new_local(String::new());
    let once_quality = RwSignal::new_local(String::new());

    let update = Rc::clone(&intents);
    let dismiss = Rc::clone(&intents);
    let channels_intents = Rc::clone(&intents);
    let filter_intents = Rc::clone(&intents);
    let recordings_intents = Rc::clone(&intents);
    let settings_intents = Rc::clone(&intents);

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
                // The video surface is the next milestone. A tab that changes
                // nothing is worse than a tab that says it cannot be used yet,
                // so it is drawn and never selected.
                box(class = "tab tab--off", attr:aria-disabled = "true") {
                    text {"Watch"}
                }
                spacer()
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
                )
            }
            if move || tab.get() == Tab::Filter {
                FilterTab(
                    words = words_sig,
                    draft = word_draft,
                    intents = Rc::clone(&filter_intents),
                )
            }
            if move || tab.get() == Tab::Recordings {
                RecordingsTab(
                    pipelines = pipelines_sig,
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
                    notifications = notifications_sig,
                    logs = logs_sig,
                    pipelines = pipelines_sig,
                )
            }
            if move || tab.get() == Tab::Settings {
                SettingsTab(
                    title_changes = title_sig,
                    sound = sound_sig,
                    display = Rc::clone(&display),
                    intents = Rc::clone(&settings_intents),
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
    intents: Rc<IntentSender>,
    toggle_pin: UnsyncCallback<u64>,
) -> impl IntoView {
    let add_login: Rc<dyn Fn()> = {
        let intents = Rc::clone(&intents);
        Rc::new(move || {
            let login = draft.get_untracked().trim().to_owned();
            if login.is_empty() {
                return;
            }
            intents.send(Request::AddLogin {
                login,
                switches: None,
            });
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
                    for channel in move || (*channels.get()).clone(), key = |channel: &RwSignal<ChannelView, LocalStorage>| channel.get_untracked().id {
                        ChannelRow(
                            channel = channel,
                            pins = pins,
                            pipelines = pipelines,
                            now = now,
                            expanded = expanded,
                            closing = closing,
                            intents = Rc::clone(&rows),
                            toggle_pin = toggle_pin,
                        )
                    }
                    for login in move || (*pending.get()).clone(), key = |login: &String| login.clone() {
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
) -> impl IntoView {
    // Nothing below reads `channel` directly: the component body runs once, so
    // every value that has to follow the daemon is a derived signal read by the
    // hole that draws it.
    let id = channel.get_untracked().id;
    let login = channel.get_untracked().login.clone();
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
    // A recording is claimed by the login, not the row: the pipeline key is
    // the monitor's (login, quality), while the row knows the channel.
    let recording = Signal::derive_local(move || {
        pipelines.get().iter().any(|pipeline| {
            pipeline.with(|pipeline| {
                pipeline.key.login.eq_ignore_ascii_case(&login) && is_recorded(pipeline)
            })
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

    let live_intents = Rc::clone(&intents);
    let title_intents = Rc::clone(&intents);
    let record_intents = Rc::clone(&intents);
    let detail_intents = Rc::clone(&intents);

    // The three switches are controlled: the click sends the intent and the
    // switch moves when the daemon's word on it comes back, so a rejected
    // change cannot sit on screen looking applied.
    let notify_live = Binding::controlled(
        Signal::derive_local(move || channel.with(|c| c.notify_live)),
        move |on: bool| {
            live_intents.send(Request::SetNotifyLive {
                channel: id,
                notify: on,
            });
        },
    );
    let notify_titles = Binding::controlled(
        Signal::derive_local(move || channel.with(|c| c.notify_titles)),
        move |on: bool| {
            title_intents.send(Request::SetNotifyTitles {
                channel: id,
                notify: on,
            });
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
            remove_channel(
                Rc::clone(&intents),
                &channel.get_untracked(),
                toasts,
                pinned.get_untracked(),
                toggle_pin,
            );
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
                                        // An inline transform rather than a custom property
                                        // read back from the stylesheet: a bare `var()` value
                                        // works (the channel accents prove it), but the same
                                        // substitution inside `translateX()` was dropped and
                                        // the title never moved.
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
/// the undo is a real re-add carrying the switches the row had. `AddLogin`
/// applies them when the resolve lands, so the row never shows the defaults.
fn remove_channel(
    intents: Rc<IntentSender>,
    view: &ChannelView,
    toasts: Option<ToastQueue>,
    pinned: bool,
    toggle_pin: UnsyncCallback<u64>,
) {
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
    let switches = Some(ChannelSwitches {
        notify_live: view.notify_live,
        notify_titles: view.notify_titles,
        record: view.record,
    });
    toasts.push(
        Toast::new(format!("Removed {name}"))
            .duration(Duration::from_secs(8))
            .action("Undo", move || {
                intents.send(Request::AddLogin {
                    login: login.clone(),
                    switches,
                });
                // The config prune drops the pin the moment the channel
                // leaves, so the undo has to put it back explicitly.
                if pinned {
                    toggle_pin.run(id);
                }
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
                        remove_channel(
                            Rc::clone(&remove_intents),
                            &remove_view,
                            toasts,
                            pinned.get_untracked(),
                            toggle_pin,
                        );
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
    words: RwSignal<Rc<Vec<String>>, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let add_word: Rc<dyn Fn()> = {
        let intents = Rc::clone(&intents);
        Rc::new(move || {
            let word = draft.get_untracked().trim().to_owned();
            if word.is_empty() {
                return;
            }
            intents.send(Request::AddFilteredWord(word));
            draft.set(String::new());
        })
    };
    let word_rows = Rc::clone(&intents);

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
                    for word in move || (*words.get()).clone(), key = |word: &String| word.clone() {
                        WordRowView(word = word, intents = Rc::clone(&word_rows))
                    }
                }
            }
        }
    }
}

/// Everything that is a preference rather than a list.
///
/// Two sections: the screen the window is on (the client's own settings) and
/// what the daemon does with a notification (the daemon's).
#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn SettingsTab(
    title_changes: RwSignal<bool, LocalStorage>,
    sound: RwSignal<bool, LocalStorage>,
    display: Rc<Display>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let titles = Rc::clone(&intents);
    let sound_switch = Rc::clone(&intents);

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
                                    display.set(UiSettings {
                                        hdr: if on { HdrMode::Auto } else { HdrMode::Off },
                                        ..display.present()
                                    });
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
                                    display.set(UiSettings {
                                        tearing: on,
                                        ..display.present()
                                    });
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
                            Signal::derive_local(move || title_changes.get()),
                            move |on| {
                                titles.send(Request::SetNotifyTitleChanges(on));
                            },
                        )
                    )
                }
                row(class = "switch-row") {
                    column(class = "switch-copy") {
                        text(class = "switch-title") {"Notification sound"}
                        text(class = "switch-hint") {"Play a sound with every toast and webhook."}
                    }
                    spacer()
                    Switch(
                        checked = Binding::controlled(
                            Signal::derive_local(move || sound.get()),
                            move |on| {
                                sound_switch.send(Request::SetSound(on));
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
fn WordRowView(word: String, intents: Rc<IntentSender>) -> impl IntoView {
    let remove = word.clone();
    view! {
        row(class = "word") {
            text(class = "word__text") {{word}}
            spacer()
            Button(
                size = ButtonSize::IconSm,
                variant = ButtonVariant::Ghost,
                on:click = {
                    let intents = Rc::clone(&intents);
                    move |_| {
                        intents.send(Request::RemoveFilteredWord(remove.clone()));
                    }
                }
            ) {"×"}
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn RecordingsTab(
    pipelines: RwSignal<Rc<Vec<RwSignal<PipelineView, LocalStorage>>>, LocalStorage>,
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
                        (key.login, key.quality)
                    } {
                        RecordingRow(pipeline = pipeline, intents = Rc::clone(&rows))
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
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let key = pipeline.get_untracked().key;
    let login = key.login.clone();
    let quality = key.quality.clone().unwrap_or_else(|| "best".to_owned());
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
                text(class = "cell cell--channel") {{login}}
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
    notifications: RwSignal<Rc<Vec<NotificationView>>, LocalStorage>,
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
                    if move || !(*notifications.get()).is_empty() {
                        text(class = "section-title") {"Notifications"}
                        for note in move || (*notifications.get()).clone().into_iter().rev().collect::<Vec<_>>(), key = |note: &NotificationView| note.id {
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
                        (key.login, key.quality)
                    } {
                        row(class = "pipeline") {
                            text(class = "pipeline__name") {{move || {
                                let key = pipeline.with(|pipeline| pipeline.key.clone());
                                format!("{}{}", key.login, key.quality.as_ref().map_or_else(String::new, |quality| format!(" · {quality}")))
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
    notifications: &[NotificationView],
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
    items.extend(notifications.iter().rev().take(8).map(|note| {
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
    // A title that fits its cell does not scroll: there is no copy to advance.
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
            if pipeline.key.login.eq_ignore_ascii_case(&channel.login) {
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

/// The token block the zgui-ui controls read. It is laid over the dark preset
/// rather than written as a second style sheet so the provider owns the cascade
/// and every control is themed by one palette.
const THEME_CSS: &str = r"
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

const SHEET: &str = css!(
    r####"
:root {
        background-color: oklch(0.16 0.012 240);
        color: oklch(0.95 0.008 240);
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
        gap: 0;
        align-items: center;
        /* Full-bleed rule under the tabs: the chrome above is inset, the
           divider between it and the content is not. */
        margin: 0 -12px;
        padding: 0 14px;
        border-bottom: 1px solid oklch(0.32 0.020 240);
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
        color: oklch(0.70 0.018 240);
        font-size: 13px;
        cursor: pointer;
    }

    .tab:hover {
        color: oklch(0.95 0.008 240);
        background-color: oklch(0.25 0.016 240);
    }

    .tab.on {
        color: #ffffff;
        background-color: oklch(0.24 0.016 240);
        border-color: oklch(0.30 0.018 240);
    }

    .tab--off {
        color: oklch(0.45 0.020 240);
        cursor: not-allowed;
    }

    .tab--off:hover {
        color: oklch(0.45 0.020 240);
        background-color: transparent;
    }

    .tabbody {
        flex-grow: 1;
        gap: 10px;
        padding: 12px;
        border-radius: 12px;
        border: 1px solid oklch(0.30 0.018 240);
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
        border-bottom: 1px solid oklch(0.32 0.020 240);
    }

    .tablehead .cell {
        color: oklch(0.62 0.020 240);
        font-size: 10px;
        font-weight: 600;
        letter-spacing: 0.6px;
        white-space: nowrap;
    }

    .cell {
        font-size: 12px;
        color: oklch(0.88 0.010 240);
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
        color: oklch(0.62 0.020 240);
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
           row's own colour without changing the layout box (a negative margin
           there made the whole scroll area stop scrolling). */
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
       out except at rest, which made a pulsing "●" disappear. */
    .live-dot__lamp {
        width: 6px;
        height: 6px;
        background-color: oklch(0.40 0.015 240);
    }

    .live-dot.live .live-dot__lamp {
        background-color: #f43f5e;
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

    /* The one display face in the table: the old wordmark's Chillax. */
    .channel-name {
        flex-grow: 1;
        min-width: 0;
        font-family: BrandFont, Chillax, sans-serif;
        font-variation-settings: "wght" 600, "wdth" 100;
        font-size: 14px;
        font-weight: 600;
        color: oklch(0.95 0.008 240);
        overflow: hidden;
        text-overflow: ellipsis;
        white-space: nowrap;
    }

    .channel-title {
        width: 100%;
        height: 18px;
        font-size: 13px;
        color: oklch(0.70 0.018 240);
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
        color: #ff9aa8;
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
        color: oklch(0.88 0.010 240);
    }

    .viewers__value.stale { color: oklch(0.60 0.015 240); }

    /* The tooltip trigger is an inline-flex box, so `text-align` on the cell
       never reaches its text: the value sat at the left of a 92px cell while
       the header was right-aligned in its own. Push the one child to the end
       instead. */
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
           an inset `box-shadow` would have been the obvious way to draw it,
           but this engine paints only the first shadow in a list and the
           wrapper already spends its one on the gutter strip. */
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
        color: #ff9aa8;
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
        color: oklch(0.62 0.020 240);
    }

    .badge {
        padding: 3px 10px;
        border-radius: 999px;
        font-size: 10px;
        font-weight: 700;
        letter-spacing: 0.6px;
        background-color: oklch(0.24 0.016 240);
        color: oklch(0.62 0.020 240);
    }

    .badge.live {
        background-color: #3a1420;
        color: #ff6b81;
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
        color: oklch(0.62 0.020 240);
    }

    .detail-value, .detail-value__text {
        font-size: 12px;
        color: oklch(0.88 0.010 240);
    }
    .detail-value { flex-grow: 1; }

    .settings { gap: 8px; }

    .switch-row {
        align-items: center;
        gap: 12px;
        padding: 12px 14px;
        border-radius: 10px;
        border: 1px solid oklch(0.30 0.018 240);
        background-color: oklch(0.20 0.013 240);
    }

    .switch-copy { gap: 2px; }

    .switch-title {
        font-size: 14px;
        font-weight: 600;
    }

    .switch-hint {
        font-size: 12px;
        color: oklch(0.62 0.020 240);
    }

    .section-title {
        padding-top: 6px;
        font-size: 11px;
        font-weight: 700;
        letter-spacing: 0.7px;
        color: oklch(0.70 0.018 240);
    }

    .section-hint {
        font-size: 12px;
        color: oklch(0.56 0.020 240);
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
        color: oklch(0.88 0.010 240);
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
        color: #ff9aa8;
    }

    .empty {
        padding: 22px 12px;
        font-size: 13px;
        color: oklch(0.62 0.020 240);
    }

    .stats { gap: 8px; }

    .stat {
        flex-grow: 1;
        flex-basis: 0;
        gap: 2px;
        padding: 10px 12px;
        border-radius: 10px;
        border: 1px solid oklch(0.30 0.018 240);
        background-color: oklch(0.20 0.013 240);
    }

    .stat__label {
        font-size: 10px;
        font-weight: 600;
        letter-spacing: 0.6px;
        color: oklch(0.62 0.020 240);
    }

    .stat__value {
        font-size: 15px;
        font-weight: 600;
        color: oklch(0.95 0.008 240);
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
        color: oklch(0.95 0.008 240);
    }

    .notification__text {
        font-size: 12px;
        color: oklch(0.70 0.018 240);
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
        color: oklch(0.70 0.018 240);
    }

    .pipeline__meta {
        font-size: 12px;
        color: oklch(0.62 0.020 240);
    }

    .pipeline__rec {
        display: none;
        padding: 1px 8px;
        border-radius: 999px;
        background-color: #3a1420;
        color: #ff6b81;
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
        color: oklch(0.56 0.020 240);
    }

    .log__target {
        width: 74px;
        font-size: 11px;
        color: var(--zui-color-primary);
    }

    .log__message {
        flex-grow: 1;
        font-size: 12px;
        color: oklch(0.88 0.010 240);
    }

    .log.bad .log__message { color: #ff9aa8; }

    .log__meta {
        font-size: 11px;
        color: oklch(0.62 0.020 240);
    }

/* ---- first pass: dense desktop tool -------------------------------- */
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
   changes, which painted the wrong label into a tab. */
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
    color: oklch(0.70 0.018 240);
}
/* The row truncates the title to keep Notify aligned; the expanded panel is
   where the whole thing goes, wrapped rather than clipped. */
.detail-title {
    width: 100%;
    font-size: 13px;
    line-height: 1.4;
    color: oklch(0.95 0.008 240);
    white-space: normal;
    overflow-wrap: anywhere;
}
.detail-error {
    width: 100%;
    font-size: 12px;
    line-height: 1.4;
    color: #ff9aa8;
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
    border-right: 1px solid oklch(0.50 0.020 240);
    border-bottom: 1px solid oklch(0.50 0.020 240);
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
/* The old wordmark row is now a news ticker for the daemon's event stream.
   Only translate is animated: in this pinned zgui, glyphs under a scale
   transform are dropped except at the rest keyframe. Eight copies sliding by
   one copy (-12.5%) keep the loop seamless and the window filled. */
.app { padding: 14px 12px 10px; gap: 6px; }
.header {
    height: 30px;
    min-height: 0;
    padding: 0 10px;
    background-color: oklch(0.12 0.010 240);
    border-bottom: 1px solid oklch(0.32 0.020 240);
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
