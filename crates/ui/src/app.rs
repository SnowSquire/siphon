//! The ZGUI view: header, channels, filters, and the DXGI renderer setup.

use std::rc::Rc;
use std::sync::Arc;

use raw_window_handle::HasWindowHandle as _;
use siphon_rpc::ChannelView;
use siphon_rpc::ConnHealth;
use siphon_rpc::Request;
use siphon_rpc::Snapshot;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::render::GpuUnavailable;
use zgui::render::RenderTarget;
use zgui::render::Renderer;
use zgui::runtime::AppError;
use zgui::runtime::use_window;
use zgui::runtime::use_windows;
use zgui::surface::wgpu;
use zgui::view::set_interval;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::SharedGraphics;
use zgui_ui::prelude::*;

use crate::dxgi::DxgiSwapchain;
use crate::dxgi::HdrMode;
use crate::dxgi::hwnd_from_raw;
use crate::format::format_iso_ms;
use crate::placebo::Placebo;
use crate::renderer::UiRenderer;
use crate::rpc::IntentSender;
use crate::rpc::UiCommand;
use crate::rpc::UiEvent;
use crate::rpc::apply_update;
use crate::rpc::empty_snapshot;
use crate::tray::TrayAction;

pub fn run(
    events: kanal::Receiver<UiEvent>,
    tray: kanal::Receiver<TrayAction>,
    intents: IntentSender,
) -> anyhow::Result<()> {
    let (gpu, _decode) = crate::gpu::create()?;
    let graphics = SharedGraphics::with_gpu(Arc::clone(&gpu));
    let factory_gpu = Arc::clone(&gpu);
    let snapshot = RwSignal::new_local(empty_snapshot());

    app()
        .with_application_id("com.iken.siphon")
        .with_title("Siphon")
        .with_size(1000.0, 760.0)
        .with_resizable(true)
        .with_stylesheet(SHEET)
        .with_renderer(Box::new(move |surface, target| {
            dxgi_renderer(&graphics, &factory_gpu, surface, target)
        }))
        .run(move || {
            view! {
                App(
                    snapshot = snapshot,
                    intents = intents.clone(),
                    events = events.clone(),
                    tray = tray.clone(),
                )
            }
        })
        .map_err(|error| anyhow::anyhow!("zgui: {error}"))
}

fn dxgi_renderer(
    graphics: &SharedGraphics,
    gpu: &Arc<Gpu>,
    surface: &Arc<dyn zgui::platform::Surface>,
    target: RenderTarget,
) -> Result<Box<dyn Renderer>, AppError> {
    let gpu_surface = Arc::clone(surface)
        .gpu_shared()
        .ok_or_else(|| dxgi_error("the surface has no native window handle"))?;
    let window = gpu_surface
        .window_handle()
        .map_err(|error| dxgi_error(format!("window handle: {error}")))?;
    let hwnd = hwnd_from_raw(window.as_raw()).map_err(|error| dxgi_error(format!("{error:#}")))?;
    let width = u32::try_from(target.size.width)
        .map_err(|_| dxgi_error("the target width is not positive"))?;
    let height = u32::try_from(target.size.height)
        .map_err(|_| dxgi_error("the target height is not positive"))?;

    let swapchain = DxgiSwapchain::new(gpu, hwnd, width, height, HdrMode::Off)
        .map_err(|error| dxgi_error(format!("{error:#}")))?;
    let ui = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("siphon.ui"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });

    let placebo =
        Placebo::import(gpu).map_err(|error| dxgi_error(format!("libplacebo: {error:#}")))?;
    let renderer = graphics
        .renderer_supplied(target, vec![ui.clone()])
        .map_err(AppError::from)?;
    let renderer = UiRenderer::new(
        renderer,
        swapchain,
        placebo,
        ui,
        graphics.clone(),
        Arc::clone(gpu),
        hwnd,
    )
    .map_err(|error| dxgi_error(format!("{error:#}")))?;
    Ok(Box::new(renderer))
}

fn dxgi_error(reason: impl Into<String>) -> AppError {
    AppError::GpuUnavailable(GpuUnavailable::new().rejected("DXGI", reason))
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn App(
    snapshot: RwSignal<Snapshot, LocalStorage>,
    intents: IntentSender,
    events: kanal::Receiver<UiEvent>,
    tray: kanal::Receiver<TrayAction>,
) -> impl IntoView {
    let intents = Rc::new(intents);

    // The RPC worker and the tray feed channels; this interval drains them on
    // the UI thread and only touches the signal when something changed.
    let timer = set_interval(std::time::Duration::from_millis(100), move || {
        let mut state = snapshot.get_untracked();
        let mut changed = false;
        while let Ok(Some(event)) = events.try_recv() {
            match event {
                UiEvent::Snapshot(update) => state = *update,
                UiEvent::Update(update) => apply_update(&mut state, update),
            }
            changed = true;
        }
        if changed {
            snapshot.set(state);
        }
        while let Ok(Some(action)) = tray.try_recv() {
            match action {
                TrayAction::Show => use_window().focus(),
                TrayAction::Quit => use_windows().quit(),
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
        }
    });
    // The interval lives as long as the window; the component has no teardown
    // hook that owns it.
    std::mem::forget(timer);
    let draft = RwSignal::new_local(String::new());
    let word_draft = RwSignal::new_local(String::new());
    let expanded = RwSignal::new_local(None::<u64>);

    let add_login = {
        let intents = Rc::clone(&intents);
        move || {
            let login = draft.get_untracked().trim().to_owned();
            if login.is_empty() {
                return;
            }
            intents.send(UiCommand::Request(Request::AddLogin(login)));
            draft.set(String::new());
        }
    };
    let add_word = {
        let intents = Rc::clone(&intents);
        move || {
            let word = word_draft.get_untracked().trim().to_owned();
            if word.is_empty() {
                return;
            }
            intents.send(UiCommand::Request(Request::AddFilteredWord(word)));
            word_draft.set(String::new());
        }
    };

    let update = Rc::clone(&intents);
    let dismiss = Rc::clone(&intents);
    let titles = Rc::clone(&intents);
    let sound = Rc::clone(&intents);
    let rows = Rc::clone(&intents);
    let words = Rc::clone(&intents);

    view! {
        column(class = "app") {
            row(class = "header") {
                text(class = "brand") {"Siphon"}
                text(class = "conn") {{move || conn_label(snapshot.get().conn)}}
                spacer()
                if move || snapshot.get().update.is_some() {
                    row(class = "banner") {
                        text {{move || snapshot.get().update.map_or_else(String::new, |update| format!("Update to {}", update.version))}}
                        Button(
                            variant = ButtonVariant::Outline,
                            on:click = {
                                let update = Rc::clone(&update);
                                move |_| update.send(UiCommand::Request(Request::ApplyUpdate))
                            }
                        ) {"Install"}
                    }
                }
            }

            if move || snapshot.get().error.is_some() {
                row(class = "error") {
                    text(class = "error__text") {{move || snapshot.get().error.clone().unwrap_or_default()}}
                    spacer()
                    Button(
                        size = ButtonSize::Sm,
                        variant = ButtonVariant::Ghost,
                        on:click = {
                            let dismiss = Rc::clone(&dismiss);
                            move |_| dismiss.send(UiCommand::Request(Request::ClearError))
                        }
                    ) {"Dismiss"}
                }
            }

            row(class = "add") {
                Input(value = draft, placeholder = "twitch login")
                Button(on:click = move |_| add_login()) {"Add"}
            }

            ScrollArea(class = "channels") {
                column(class = "list") {
                    for channel in move || snapshot.get().channels, key = |channel: &ChannelView| channel.id {
                        ChannelRow(
                            channel = channel.clone(),
                            expanded = expanded,
                            intents = Rc::clone(&rows),
                        )
                    }
                    for login in move || snapshot.get().pending, key = |login: &String| login.clone() {
                        row(class = "channel pending") {
                            text(class = "channel__name") {{login.clone()}}
                            text(class = "channel__title") {"resolving…"}
                        }
                    }
                }
            }

            row(class = "options") {
                Button(
                    variant = ButtonVariant::Outline,
                    on:click = move |_| {
                        let on = snapshot.get_untracked().config.notify_title_changes;
                        titles.send(UiCommand::Request(Request::SetNotifyTitleChanges(!on)));
                    }
                ) {
                    text {
                        {move || if snapshot.get().config.notify_title_changes { "Titles: on" } else { "Titles: off" }}
                    }
                }
                Button(
                    variant = ButtonVariant::Outline,
                    on:click = move |_| {
                        let on = snapshot.get_untracked().config.sound;
                        sound.send(UiCommand::Request(Request::SetSound(!on)));
                    }
                ) {
                    text {
                        {move || if snapshot.get().config.sound { "Sound: on" } else { "Sound: off" }}
                    }
                }
            }

            row(class = "add") {
                Input(value = word_draft, placeholder = "filtered word")
                Button(on:click = move |_| add_word()) {"Add"}
            }

            ScrollArea(class = "words") {
                column(class = "list") {
                    for word in move || {
                        snapshot
                            .get()
                            .config
                            .filtered_words
                            .iter()
                            .enumerate()
                            .map(|(index, word)| WordRow {
                                index: u32::try_from(index).unwrap_or(u32::MAX),
                                text: word.clone(),
                            })
                            .collect::<Vec<_>>()
                    }, key = |word: &WordRow| word.index {
                        WordRowView(word = word.clone(), intents = Rc::clone(&words))
                    }
                }
            }

        }
    }
}

#[derive(Clone, Debug)]
struct WordRow {
    index: u32,
    text: String,
}

#[component]
#[allow(clippy::needless_pass_by_value)]
fn ChannelRow(
    channel: ChannelView,
    expanded: RwSignal<Option<u64>, LocalStorage>,
    intents: Rc<IntentSender>,
) -> impl IntoView {
    let id = channel.id;
    let live = channel.live;
    view! {
        column(class = "channel") {
            row(
                class = "channel__line",
                on:click = move |_| {
                    expanded.update(|expanded| {
                        *expanded = if *expanded == Some(id) { None } else { Some(id) };
                    });
                }
            ) {
                text(class = if live { "dot dot--live" } else { "dot" }) {"●"}
                text(class = "channel__name") {{channel.display_name.clone()}}
                text(class = "channel__title") {{channel.title.clone().unwrap_or_default()}}
                spacer()
                text(class = "channel__viewers") {{viewers(&channel)}}
                Button(
                    size = ButtonSize::IconSm,
                    variant = ButtonVariant::Ghost,
                    on:click:stop = {
                        let intents = Rc::clone(&intents);
                        move |_| intents.send(UiCommand::Request(Request::RemoveChannel(id)))
                    }
                ) {"×"}
            }
            if move || expanded.get() == Some(id) {
                text(class = "channel__detail") {{detail(&channel)}}
            }
        }
    }
}

#[component]
#[allow(clippy::needless_pass_by_value, clippy::redundant_clone)]
fn WordRowView(word: WordRow, intents: Rc<IntentSender>) -> impl IntoView {
    let index = word.index;
    view! {
        row(class = "word") {
            text {{word.text.clone()}}
            spacer()
            Button(
                size = ButtonSize::IconSm,
                variant = ButtonVariant::Ghost,
                on:click = {
                    let intents = Rc::clone(&intents);
                    move |_| intents.send(UiCommand::Request(Request::RemoveFilteredWord(index)))
                }
            ) {"×"}
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

fn viewers(channel: &ChannelView) -> String {
    if !channel.live {
        return "offline".to_owned();
    }
    channel
        .viewers
        .map_or_else(|| "—".to_owned(), |viewers| viewers.to_string())
}

fn detail(channel: &ChannelView) -> String {
    let game = channel.game.as_deref().unwrap_or("no game");
    let start = channel
        .stream_start_ms
        .map_or_else(|| "—".to_owned(), format_iso_ms);
    let count =
        |value: Option<u32>| value.map_or_else(|| "—".to_owned(), |value| value.to_string());
    format!(
        "{} • {} • {} • {}/{}",
        channel.login,
        game,
        start,
        count(channel.viewers),
        count(channel.collaboration_viewers)
    )
}

const SHEET: &str = css!(
    ":root {
        background-color: #12141a;
        color: #e8ecf4;
        font-family: sans-serif;
    }

    .app {
        padding: 16px 18px;
        gap: 10px;
        flex-grow: 1;
    }

    .header {
        align-items: center;
        gap: 12px;
    }

    .brand {
        font-size: 22px;
        font-weight: 700;
    }

    .conn {
        color: #7d879b;
        font-size: 12px;
    }

    .banner {
        align-items: center;
        gap: 8px;
        padding: 4px 8px;
        border-radius: 8px;
        background-color: #1c2a1e;
        color: #b9e4bd;
        font-size: 13px;
    }

    .error {
        align-items: center;
        gap: 8px;
        padding: 8px 10px;
        border-radius: 8px;
        background-color: #3a1d22;
        color: #f0b8be;
        font-size: 13px;
    }

    .error__text { flex-grow: 1; }

    .add {
        gap: 8px;
        align-items: center;
    }

    .channels {
        height: 360px;
        border-radius: 10px;
        border: 1px solid #262b36;
        background-color: #171b24;
    }

    .words {
        height: 110px;
        border-radius: 10px;
        border: 1px solid #262b36;
        background-color: #171b24;
    }

    .list { gap: 2px; padding: 6px; }

    .channel { padding: 2px 4px; }

    .channel__line {
        align-items: center;
        gap: 8px;
        padding: 6px 8px;
        border-radius: 7px;
    }

    .channel__line:hover { background-color: #1e2430; }

    .dot {
        color: #555b68;
        font-size: 11px;
    }

    .dot--live { color: #2ea043; }

    .channel__name {
        width: 140px;
        font-size: 15px;
    }

    .channel__title {
        flex-grow: 1;
        font-size: 13px;
        color: #9aa4b6;
    }

    .channel__viewers {
        font-size: 13px;
        color: #9aa4b6;
    }

    .channel__detail {
        padding-left: 30px;
        padding-bottom: 4px;
        font-size: 12px;
        color: #7d879b;
    }

    .pending .channel__title { color: #7d879b; }

    .options { gap: 8px; }

    .word {
        align-items: center;
        gap: 8px;
        padding: 4px 8px;
        font-size: 13px;
    }

    .pump {
        width: 1px;
        height: 1px;
    }"
);
