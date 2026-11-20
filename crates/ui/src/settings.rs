//! The display settings this client keeps for itself.
//!
//! The renderer reads them, the preferences section writes them, and a change is
//! persisted the moment it is made. They describe the screen the window is on,
//! so they belong to this client rather than to the daemon: a second UI attached
//! to the same daemon is free to differ, and one day a remote one will.

use std::rc::Rc;

use siphon_shared::settings::UiSettings;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;

use crate::service::APP_ID;

/// The settings a window is presented with.
pub struct Display {
    settings: RwSignal<UiSettings, LocalStorage>,
}

impl Display {
    /// The settings stored for this client, or the defaults on a first run.
    pub fn load() -> Rc<Self> {
        let settings = siphon_shared::settings::load(APP_ID);
        Rc::new(Self {
            settings: RwSignal::new_local(settings),
        })
    }

    /// The settings as the renderer reads them.
    ///
    /// Untracked on purpose: the renderer is not a reactive scope, and a read
    /// there must neither subscribe anything nor re-run a frame.
    pub fn present(&self) -> UiSettings {
        self.settings.get_untracked()
    }

    /// The settings as the view reads them, so a switch follows a change made
    /// anywhere else.
    pub fn get(&self) -> UiSettings {
        self.settings.get()
    }

    /// Changes the settings, keeping the file and the screen in step.
    pub fn set(&self, settings: UiSettings) {
        self.settings.set(settings);
        if let Err(error) = siphon_shared::settings::save(APP_ID, &settings) {
            tracing::warn!(target: "settings", "could not save display settings: {error}");
        }
    }
}
