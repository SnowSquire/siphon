//! The ZGUI view: header, tabs, channels, filters, recordings, logs, and the
//! renderer setup.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use siphon_rpc::ArenaSummary;
use siphon_rpc::ChannelView;
use siphon_rpc::ConnHealth;
use siphon_rpc::DownloaderSummary;
use siphon_rpc::LogEntry;
use siphon_rpc::LogLevel;
use siphon_rpc::NotificationView;
use siphon_rpc::PipelineState;
use siphon_rpc::PipelineView;
use siphon_rpc::Request;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::reactive::Signal;
use zgui::reactive::UnsyncCallback;
#[cfg(windows)]
use zgui::runtime::use_window;
#[cfg(windows)]
use zgui::runtime::use_windows;
use zgui_ui::prelude::*;
use zgui_ui_tokens::prelude::*;

use crate::format::format_bytes;
use crate::format::format_count;
use crate::format::format_duration_ms;
use crate::format::format_iso_ms;
use crate::format::format_time_ms;
use crate::rpc::IntentSender;
use crate::rpc::UiEvent;
use crate::rpc::UiState;
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
}

pub fn run(events: kanal::Receiver<UiEvent>, intents: IntentSender) -> eros::Result<()> {
    let state = UiState::new();
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

    // Shipped fonts are registered by path in dev so typography can be tried
    // without a rebuild: SIPHON_FONT_UI, SIPHON_FONT_BRAND. Release builds
    // keep ZGUI's system fonts until the licensing/asset path is settled.
    let fonts = fonts()?;
    let window = window.with_fonts(fonts);

    // Both platforms compose the interface into a texture and present it
    // through a chain: DXGI on Windows, a Vulkan swapchain on Wayland. Only the
    // chain differs, and it is chosen inside the renderer factory.
    let window = {
        let (gpu, _decode) = crate::gpu::create()?;
        let graphics = zgui_render_wgpu::SharedGraphics::with_gpu(Arc::clone(&gpu));
        let factory_gpu = Arc::clone(&gpu);
        window.with_renderer(Box::new(move |surface, target| {
            crate::chain::renderer(&graphics, &factory_gpu, surface, target)
        }))
    };

    window
        .run(move || {
            // Fresh handles per build: the run closure is `FnMut`, so it must
            // not move its captures into the first view it makes.
            let state = state.clone();
            let intents = intents.clone();
            let events = events.clone();
            let dark = dark;
            view! {
                TooltipProvider(delay = Duration::from_millis(400)) {
                    ThemeProvider(scheme = ColorScheme::Dark, dark = dark) {
                        App(state = state, intents = intents, events = events)
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
fn App(state: UiState, intents: IntentSender, events: kanal::Receiver<UiEvent>) -> impl IntoView {
    let intents = Rc::new(intents);
    // One handle per signal the view reads, so a closure depends on that field
    // alone: ZGUI reruns it when the signals it read change, not when any part of
    // the snapshot does.
    let conn_sig = state.conn;
    let error_sig = state.error;
    let offer = state.update;
    let channels_sig = state.channels;
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
    let tab_weight = RwSignal::new_local(1.0f32);
    let previous_tab = RwSignal::new_local(None::<Tab>);
    let tab_generation = Rc::new(Cell::new(0u64));
    let tab = RwSignal::new_local(Tab::Channels);
    let tab_click = move |target: Tab| {
        let generation = Rc::clone(&tab_generation);
        move |_: &mut EventCx<'_, events::Click>| {
            previous_tab.set(Some(tab.get_untracked()));
            tab.set(target);
            animate_tab_weight(tab_weight, Rc::clone(&generation));
        }
    };
    let expanded = RwSignal::new_local(BTreeSet::<u64>::new());
    let draft = RwSignal::new_local(String::new());
    let word_draft = RwSignal::new_local(String::new());
    let once_login = RwSignal::new_local(String::new());
    let once_quality = RwSignal::new_local(String::new());

    let update = Rc::clone(&intents);
    let dismiss = Rc::clone(&intents);
    let channels_intents = Rc::clone(&intents);
    let filter_intents = Rc::clone(&intents);
    let recordings_intents = Rc::clone(&intents);

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
                    text(class = "tab__label", var:--tab-label-w = Some("66px".to_owned())) {"Channels"}
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Filter,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Filter),
                    on:click = tab_click(Tab::Filter)
                ) {
                    text(class = "tab__label", var:--tab-label-w = Some("36px".to_owned())) {"Filter"}
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Recordings,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Recordings),
                    on:click = tab_click(Tab::Recordings)
                ) {
                    text(class = "tab__label", var:--tab-label-w = Some("74px".to_owned())) {"Recordings"}
                }
                box(
                    class = "tab",
                    class:on = move || tab.get() == Tab::Logs,
                    var:--tab-wght = tab_weight_value(tab, previous_tab, tab_weight, Tab::Logs),
                    on:click = tab_click(Tab::Logs)
                ) {
                    text(class = "tab__label", var:--tab-label-w = Some("32px".to_owned())) {"Logs"}
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
                    pending = pending_sig,
                    expanded = expanded,
                    draft = draft,
                    intents = Rc::clone(&channels_intents),
                )
            }
            if move || tab.get() == Tab::Filter {
                FilterTab(
                    words = words_sig,
                    title_changes = title_sig,
                    sound = sound_sig,
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
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn ChannelsTab(
    channels: RwSignal<Rc<Vec<ChannelView>>, LocalStorage>,
    pending: RwSignal<Rc<Vec<String>>, LocalStorage>,
    expanded: RwSignal<BTreeSet<u64>, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let add_login = {
        let intents = Rc::clone(&intents);
        move || {
            let login = draft.get_untracked().trim().to_owned();
            if login.is_empty() {
                return;
            }
            intents.send(Request::AddLogin(login));
            draft.set(String::new());
        }
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
                text(class = "cell cell--notify") {"Notify"}
                text(class = "cell cell--record") {"Record"}
                text(class = "cell cell--viewers") {"Viewers"}
                text(class = "cell cell--action") {""}
            }
            ScrollArea(class = "channels") {
                column(class = "list") {
                    for channel in move || (*channels.get()).clone(), key = |channel: &ChannelView| channel.id {
                        ChannelRow(
                            channel = channel.clone(),
                            expanded = expanded,
                            intents = Rc::clone(&rows),
                        )
                    }
                    for login in move || (*pending.get()).clone(), key = |login: &String| login.clone() {
                        row(class = "channel-row pending") {
                            text(class = "cell cell--dot") {""}
                            box(class = "live-dot") {box(class = "live-dot__lamp")}
                            text(class = "cell cell--channel") {{login.clone()}}
                            text(class = "cell cell--title") {"resolving…"}
                            spacer()
                        }
                    }
                }
            }
            row(class = "add") {
                Input(
                    value = draft,
                    placeholder = "Add a new streamer",
                    on_change = clamp_login,
                    class = "add__input",
                )
                Button(on:click = move |_| add_login()) {"Add"}
            }
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn ChannelRow(
    channel: ChannelView,
    expanded: RwSignal<BTreeSet<u64>, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let id = channel.id;
    let open = Signal::derive_local(move || expanded.get().contains(&id));
    let display_name = channel.display_name.clone();
    let initials = initials(&display_name);
    let color = channel.chat_color.clone();
    let avatar_path = channel.avatar_path.clone();
    let title = channel
        .title
        .clone()
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "—".to_owned());
    let viewers = viewers(&channel);
    let live = channel.live;
    let notify_live = RwSignal::new_local(channel.notify_live);
    let notify_titles = RwSignal::new_local(channel.notify_titles);
    let record = RwSignal::new_local(channel.record);
    let detail = channel.clone();

    // Only titles that cannot fit their cell scroll. Three copies keep the
    // cell filled; the offset is a signal so leaving the row can rewind it.
    let long_title = title.chars().count() > 55;
    let title_run = if long_title {
        format!("{title}     │     ").repeat(3)
    } else {
        title.clone()
    };
    let title_shift = RwSignal::new_local(0.0f32);
    let title_gen = Rc::new(Cell::new(0u64));
    let title_limit = if long_title {
        title.chars().count() as f32 * 6.4
    } else {
        0.0
    };
    let enter = {
        let token = Rc::clone(&title_gen);
        move |_: &mut EventCx<'_, events::PointerEnter>| {
            scroll_title(title_shift, title_limit, Rc::clone(&token), false);
        }
    };
    let leave = {
        let token = Rc::clone(&title_gen);
        move |_: &mut EventCx<'_, events::PointerLeave>| {
            scroll_title(title_shift, title_limit, Rc::clone(&token), true);
        }
    };

    let live_intents = Rc::clone(&intents);
    let title_intents = Rc::clone(&intents);
    let record_intents = Rc::clone(&intents);
    let remove_intents = Rc::clone(&intents);

    view! {
        column(class = "channel") {
            row(
                class = "channel-row",
                class:open = open,
                var:--channel-color = color,
                on:pointer_enter = enter,
                on:pointer_leave = leave,
                on:click = move |_| {
                    expanded.update(|expanded| {
                        if !expanded.remove(&id) {
                            expanded.insert(id);
                        }
                    });
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
                                src = Signal::stored_local(avatar_path.clone()),
                                label = display_name.clone()
                            ) {
                                text {{initials.clone()}}
                            }
                            text(class = "channel-name") {{display_name.clone()}}
                        }
                    }
                    TooltipContent {{format!("Channel ID {id}")}}
                }
                Tooltip {
                    TooltipTrigger(class = "cell cell--title") {
                        box(class = "channel-title") {
                            text(
                                class = "channel-title__run",
                                class:active = long_title,
                                var:--title-shift = move || Some(format!("-{:.1}px", title_shift.get())),
                            ) {{title_run}}
                        }
                    }
                    TooltipContent {{stream_tooltip(&channel)}}
                }
                box(class = "cell cell--notify", on:click:stop = |_| {}) {
                    Tooltip {
                        TooltipTrigger {
                            column(class = "mini") {
                                Switch(
                                    size = SwitchSize::Sm,
                                    checked = notify_live,
                                    on_change = UnsyncCallback::new(move |on| {
                                        live_intents.send(Request::SetNotifyLive {
                                            channel: id,
                                            notify: on,
                                        });
                                    })
                                )
                                text(class = "mini__label") {"live"}
                            }
                        }
                        TooltipContent {"Notify when this channel goes live"}
                    }
                    Tooltip {
                        TooltipTrigger {
                            column(class = "mini") {
                                Switch(
                                    size = SwitchSize::Sm,
                                    checked = notify_titles,
                                    on_change = UnsyncCallback::new(move |on| {
                                        title_intents.send(Request::SetNotifyTitles {
                                            channel: id,
                                            notify: on,
                                        });
                                    })
                                )
                                text(class = "mini__label") {"title"}
                            }
                        }
                        TooltipContent {"Notify when the title or game changes"}
                    }
                }
                box(class = "cell cell--record", on:click:stop = |_| {}) {
                    Switch(
                        size = SwitchSize::Sm,
                        checked = record,
                        on_change = UnsyncCallback::new(move |on| {
                            record_intents.send(Request::SetRecord {
                                channel: id,
                                record: on,
                            });
                        })
                    )
                }
                text(class = "cell cell--viewers") {{viewers.clone()}}
                Button(
                    size = ButtonSize::IconSm,
                    variant = ButtonVariant::Ghost,
                    class = "remove",
                    on:click:stop = move |_| {
                        remove_intents.send(Request::RemoveChannel(id));
                    }
                ) {"×"}
            }
            if move || open.get() {
                {channel_detail_view(&detail)}
            }
        }
    }
}

fn channel_detail_view(channel: &ChannelView) -> impl IntoView {
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
    let viewers = channel.viewers.map_or_else(|| "—".to_owned(), format_count);

    view! {
        column(class = "detail") {
            row(class = "detail-head") {
                text(class = "detail-login") {{format!("@{login}")}}
            }
            text(class = "detail-title") {{title}}
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
                }
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
    title_changes: RwSignal<bool, LocalStorage>,
    sound: RwSignal<bool, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let add_word = {
        let intents = Rc::clone(&intents);
        move || {
            let word = draft.get_untracked().trim().to_owned();
            if word.is_empty() {
                return;
            }
            intents.send(Request::AddFilteredWord(word));
            draft.set(String::new());
        }
    };
    let titles = Rc::clone(&intents);
    let sound_switch = Rc::clone(&intents);
    let word_rows = Rc::clone(&intents);

    view! {
        column(class = "tabbody") {
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
            text(class = "section-title") {"Filtered words"}
            text(class = "section-hint") {"A notification whose title contains one of these is dropped."}
            row(class = "add") {
                Input(value = draft, placeholder = "Add a filtered word", class = "add__input")
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
    pipelines: RwSignal<Rc<Vec<PipelineView>>, LocalStorage>,
    draft: RwSignal<String, LocalStorage>,
    quality: RwSignal<String, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let record_once = {
        let intents = Rc::clone(&intents);
        move || {
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
        }
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
                    for pipeline in move || recordings(&pipelines.get()), key = |pipeline: &PipelineView| (pipeline.key.login.clone(), pipeline.key.quality.clone()) {
                        RecordingRow(pipeline = pipeline.clone(), intents = Rc::clone(&rows))
                    }
                    if move || recordings(&pipelines.get()).is_empty() {
                        text(class = "empty") {"No recording is running. Pick a channel above and press Record once, or turn Record on in Channels."}
                    }
                }
            }
            row(class = "add") {
                Input(value = draft, placeholder = "Add a channel to record once", class = "add__input")
                Input(value = quality, placeholder = "quality (optional)", class = "add__quality")
                Button(on:click = move |_| record_once()) {"Record once"}
            }
        }
    }
}

fn recordings(pipelines: &[PipelineView]) -> Vec<PipelineView> {
    pipelines
        .iter()
        .filter(|pipeline| pipeline.recording || pipeline.disk.is_some())
        .cloned()
        .collect()
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn RecordingRow(pipeline: PipelineView, intents: Rc<IntentSender>) -> impl IntoView {
    let key = pipeline.key.clone();
    let login = key.login.clone();
    let quality = key.quality.clone().unwrap_or_else(|| "best".to_owned());
    let state = state_label(pipeline.state);
    let position = pipeline
        .disk
        .as_ref()
        .map_or_else(String::new, |disk| format_duration_ms(disk.position_ms));
    let segments = pipeline
        .disk
        .as_ref()
        .map_or_else(String::new, |disk| disk.segments.to_string());
    let size = pipeline
        .disk
        .as_ref()
        .map_or_else(String::new, |disk| format_bytes(disk.bytes));
    let runs = pipeline
        .disk
        .as_ref()
        .map_or_else(String::new, |disk| disk.runs.to_string());
    let error = pipeline
        .disk
        .as_ref()
        .and_then(|disk| disk.last_error.clone())
        .or_else(|| pipeline.last_error.clone())
        .unwrap_or_default();
    let has_error = !error.is_empty();
    let recording = pipeline.recording;

    view! {
        column(class = "recording") {
            row(class = "recording-row") {
                text(class = "cell cell--channel") {{login}}
                text(class = "cell cell--quality") {{quality}}
                text(class = "cell cell--state") {{state}}
                text(class = "cell cell--position") {{position}}
                text(class = "cell cell--segments") {{segments}}
                text(class = "cell cell--size") {{size}}
                text(class = "cell cell--runs") {{runs}}
                if move || recording {
                    Button(
                        size = ButtonSize::Sm,
                        variant = ButtonVariant::Ghost,
                        on:click = {
                            let intents = Rc::clone(&intents);
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
            if move || has_error {
                text(class = "recording__error") {{error.clone()}}
            }
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn LogsTab(
    conn: RwSignal<ConnHealth, LocalStorage>,
    channels: RwSignal<Rc<Vec<ChannelView>>, LocalStorage>,
    arena: RwSignal<ArenaSummary, LocalStorage>,
    downloader: RwSignal<DownloaderSummary, LocalStorage>,
    notifications: RwSignal<Rc<Vec<NotificationView>>, LocalStorage>,
    logs: RwSignal<Rc<Vec<LogEntry>>, LocalStorage>,
    pipelines: RwSignal<Rc<Vec<PipelineView>>, LocalStorage>,
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
                    for pipeline in move || (*pipelines.get()).clone(), key = |pipeline: &PipelineView| (pipeline.key.login.clone(), pipeline.key.quality.clone()) {
                        row(class = "pipeline") {
                            text(class = "pipeline__name") {{format!("{}{}", pipeline.key.login, pipeline.key.quality.as_ref().map_or_else(String::new, |quality| format!(" · {quality}")))}}
                            text(class = "pipeline__state") {{state_label(pipeline.state)}}
                            text(class = "pipeline__meta") {{format!("{} · {} viewers", format_duration_ms(pipeline.position_ms), pipeline.sessions)}}
                            text(class = "pipeline__rec", class:on = pipeline.recording) {"REC"}
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

fn conn_summary(conn: ConnHealth, channels: &[ChannelView]) -> String {
    let live = channels.iter().filter(|channel| channel.live).count();
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
fn ticker_text(channels: &[ChannelView], notifications: &[NotificationView]) -> String {
    let mut items = channels
        .iter()
        .filter_map(|channel| {
            channel
                .title
                .as_deref()
                .filter(|title| !title.is_empty())
                .map(|title| format!("● {} — {}", channel.display_name, title))
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
fn next_frame(
    generation: Rc<Cell<u64>>,
    token: u64,
    slot: &Rc<RefCell<Option<zgui::view::FrameHandle>>>,
    mut step: impl FnMut() -> bool + 'static,
) {
    let inner = Rc::clone(slot);
    let handle = request_frame(move |_| {
        if token != generation.get() {
            return;
        }
        if !step() {
            next_frame(Rc::clone(&generation), token, &inner, step);
        }
    });
    *slot.borrow_mut() = Some(handle);
}

/// Eases the variable weight from the old tab to the new one over 250ms. A
/// generation counter makes a quick series of tab clicks restart the tween
/// instead of letting older frames fight over the signal.
fn animate_tab_weight(weight: RwSignal<f32, LocalStorage>, generation: Rc<Cell<u64>>) {
    weight.set(0.0);
    let token = generation.get() + 1;
    generation.set(token);
    let started = std::time::Instant::now();
    let slot = Rc::new(RefCell::new(None));
    next_frame(generation, token, &slot, move || {
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
fn scroll_title(
    shift: RwSignal<f32, LocalStorage>,
    limit: f32,
    generation: Rc<Cell<u64>>,
    rewind: bool,
) {
    let token = generation.get() + 1;
    generation.set(token);
    let slot = Rc::new(RefCell::new(None));
    next_frame(generation, token, &slot, move || {
        let now = shift.get_untracked();
        let next = if rewind {
            (now - (now / 8.0 + 3.0)).max(0.0)
        } else {
            (now + 0.9).min(limit)
        };
        shift.set(next);
        (next - now).abs() < 0.01
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

fn viewers(channel: &ChannelView) -> String {
    if !channel.live {
        return "offline".to_owned();
    }
    channel.viewers.map_or_else(|| "—".to_owned(), format_count)
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
        padding: 0 2px;
        border-bottom: 1px solid oklch(0.32 0.020 240);
    }

    .tab {
        padding: 5px 14px;
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

    .cell--chevron { width: 14px; }
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
        width: 124px;
        display: flex;
        flex-direction: row;
        align-items: center;
        gap: 10px;
    }
    .cell--record {
        width: 62px;
        display: flex;
        align-items: center;
    }
    .cell--viewers { width: 82px; text-align: right; }
    .cell--action { width: 34px; }
    .cell--quality { width: 90px; }
    .cell--state { width: 82px; }
    .cell--position { width: 90px; }
    .cell--segments { width: 88px; }
    .cell--size { width: 90px; }
    .cell--runs { width: 56px; }

    .list {
        gap: 0;
        padding: 0 6px 0 0;
    }

    .channels, .recordings, .words, .logs {
        flex-grow: 1;
        min-height: 0;
    }

    .channel { padding: 0; }

    .channel-row {
        align-items: center;
        gap: 10px;
        padding: 2px 10px;
        border-radius: 0;
        min-height: 28px;
        background-color: oklch(0.18 0.012 240);
    }

    .channel-row.pending { opacity: 0.65; }

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

    .mini {
        align-items: center;
        gap: 3px;
    }

    .mini__label {
        font-size: 9px;
        color: oklch(0.62 0.020 240);
        letter-spacing: 0.3px;
    }

    .remove {
        width: 28px;
        height: 28px;
        color: oklch(0.56 0.020 240);
    }

    .detail {
        margin: 2px 6px 6px 6px;
        padding: 10px 12px;
        gap: 12px;
        border: 1px solid oklch(0.32 0.020 240);
        border-radius: 0;
        background-color: oklch(0.18 0.012 240);
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

.tab__label {
    display: inline-block;
    width: var(--tab-label-w, 60px);
    text-align: center;
}
.tabbody {
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
.channels .zui-scroll-area__viewport { overflow: scroll; }
.list { gap: 0; padding: 0 6px 0 0; }
/* Zebra rows; hover/open must follow the even-row rule to win. */
.channel:nth-child(even) .channel-row { background-color: oklch(0.21 0.014 240); }
.channel-row:hover { background-color: oklch(0.25 0.016 240); }
.channel-row.open, .channel:nth-child(even) .channel-row.open { background-color: oklch(0.25 0.016 240); }
.channel-title__run {
    display: inline-block;
    white-space: nowrap;
    line-height: 18px;
    font-size: 12px;
    color: oklch(0.70 0.018 240);
}
.channel-title__run.active {
    transform: translateX(var(--title-shift, 0px));
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
.app .zui-switch[data-size="sm"] {
    width: 40px;
    height: 22px;
    border: 1px solid var(--zui-color-border);
    border-radius: 0;
    background-color: var(--zui-color-control-switch-off);
    box-shadow: none;
}
.app .zui-switch[data-size="sm"] {
    width: 34px;
    height: 18px;
}
.app .zui-switch:checked {
    background-color: var(--channel-color, var(--zui-color-primary));
    border-color: var(--channel-color, var(--zui-color-primary));
}
.app .zui-switch > .zui-switch__thumb,
.app .zui-switch[data-size="sm"] > .zui-switch__thumb {
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
.app .zui-switch[data-size="sm"] > .zui-switch__thumb {
    width: 14px;
    height: 14px;
}
.app .zui-switch:checked > .zui-switch__thumb {
    background-color: var(--zui-color-control-switch-thumb-on);
    transform: translateX(16px);
}
.app .zui-switch[data-size="sm"]:checked > .zui-switch__thumb {
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
@keyframes ticker-scroll {
    from { transform: translateX(0); }
    to { transform: translateX(-12.5%); }
}
"####
);
