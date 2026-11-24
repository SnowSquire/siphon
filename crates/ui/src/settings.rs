//! The settings this client keeps for itself.
//!
//! One document (`ui.json`) holds the display preferences, the notification
//! policy and the per-channel switches; the renderer reads a `Copy` projection,
//! the preferences section writes the display part, and the notification
//! decision reads the rest. They describe the person and the screen, not the
//! daemon, so a second UI attached to the same daemon is free to differ.

use std::rc::Rc;

use siphon_shared::settings::ClientSettings;
use siphon_shared::settings::HdrMode;
use siphon_shared::settings::RenderSettings;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;

use crate::service::APP_ID;

/// The settings a window is presented with.
pub struct Display {
    settings: RwSignal<ClientSettings, LocalStorage>,
}

impl Display {
    /// The settings stored for this client, or the defaults on a first run.
    pub fn load() -> Rc<Self> {
        let settings = siphon_shared::settings::load(APP_ID);
        Rc::new(Self {
            settings: RwSignal::new_local(settings),
        })
    }

    /// The one handle to the document, shared with the RPC state so both write
    /// the same value and one `save` covers every change.
    #[must_use]
    pub const fn settings(&self) -> RwSignal<ClientSettings, LocalStorage> {
        self.settings
    }

    /// The settings as the renderer reads them.
    ///
    /// Untracked on purpose: the renderer is not a reactive scope, and a read
    /// there must neither subscribe anything nor re-run a frame.
    #[must_use]
    pub fn present(&self) -> RenderSettings {
        self.settings.with_untracked(ClientSettings::render)
    }

    /// The settings as the view reads them, so a switch follows a change made
    /// anywhere else.
    #[must_use]
    pub fn get(&self) -> RenderSettings {
        self.settings.with(ClientSettings::render)
    }

    /// Changes the display settings, keeping the file and the screen in step.
    pub fn set(&self, hdr: HdrMode, tearing: bool) {
        update(self.settings, |settings| {
            settings.hdr = hdr;
            settings.tearing = tearing;
        });
    }
}

/// Mutates the client settings, then writes the whole document.
///
/// The document is small and rewritten whole: a field-by-field save would need
/// one path per switch and could still lose a concurrent one.
pub fn update(
    settings: RwSignal<ClientSettings, LocalStorage>,
    change: impl FnOnce(&mut ClientSettings),
) {
    let mut document = settings.get_untracked();
    change(&mut document);
    settings.set(document.clone());
    if let Err(error) = siphon_shared::settings::save(APP_ID, &document) {
        tracing::warn!(target: "settings", "could not save settings: {error}");
    }
}
