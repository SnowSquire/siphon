//! The settings a client keeps for itself.
//!
//! These are per-client, not per-daemon: a display setting belongs to the
//! screen the window is on, so two UIs attached to one daemon — over a pipe
//! today, over a network one day — must be able to differ. That is why they
//! live in their own file instead of the daemon's config, which travels with
//! the broadcast setup rather than with a display.

use std::path::PathBuf;

use sonic_rs::Deserialize;
use sonic_rs::Serialize;

use crate::store;

/// The file under the app id's configuration directory.
pub const FILE: &str = "ui.json";

/// How a chain asks the display for high dynamic range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HdrMode {
    /// Leave the display in SDR. The switch in Preferences writes this when it
    /// is turned off, for a display left in HDR that something cannot capture
    /// from.
    Off,
    /// Use PQ/Rec.2020 when the display already reports HDR for this output.
    /// The default: the interface is then graded to the display instead of being
    /// handed to the compositor's SDR-in-HDR mapping.
    #[default]
    Auto,
    /// Ask for PQ/Rec.2020 regardless; a display that refuses is an error
    /// rather than a silent fallback.
    On,
}

/// What a client persists for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UiSettings {
    pub version: u64,
    /// The display mode the chain asks for.
    pub hdr: HdrMode,
    /// Present without waiting for a vertical blank.
    ///
    /// The newest frame then wins instead of a queue of older ones being shown
    /// in order, and the display is free to vary its refresh rate. On a
    /// fixed-rate display it may tear; the frame the compositor is showing is
    /// the one that is torn, never a stale one.
    pub tearing: bool,
}

impl UiSettings {
    /// Current on-disk schema.
    pub const VERSION: u64 = 1;
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            hdr: HdrMode::default(),
            tearing: false,
        }
    }
}

/// One client's settings file.
#[must_use]
pub fn path(app_id: &str) -> PathBuf {
    store::path(app_id, FILE)
}

/// Reads a client's settings.
///
/// A file that cannot be read, or one written by a newer build, is a fresh
/// default: display settings must never be the reason a window does not open.
#[must_use]
pub fn load(app_id: &str) -> UiSettings {
    let path = path(app_id);
    match store::read::<UiSettings>(&path) {
        Ok(Some(settings)) if settings.version <= UiSettings::VERSION => settings,
        Ok(Some(settings)) => {
            tracing::info!(
                target: "settings",
                "ignoring settings version {} at {path:?}",
                settings.version
            );
            UiSettings::default()
        }
        Ok(None) => UiSettings::default(),
        Err(error) => {
            tracing::info!(target: "settings", "starting with fresh settings: {error}");
            UiSettings::default()
        }
    }
}

/// Writes a client's settings, replacing the file.
pub fn save(app_id: &str, settings: &UiSettings) -> Result<(), store::StoreError> {
    store::write(&path(app_id), settings)
}

/// The channels a client keeps at the top of the list.
///
/// A pin is an ordering preference, not something the daemon acts on, so it
/// lives with the client's display settings rather than in the daemon's config.
/// Keyed by channel id, which is Twitch's and survives a rename; a snapshot
/// prunes ids that are no longer followed. A separate document from
/// [`UiSettings`] because that one is `Copy` and travels to the renderer, while
/// this is a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Pins {
    pub version: u64,
    pub ids: Vec<u64>,
}

impl Pins {
    /// Current on-disk schema.
    pub const VERSION: u64 = 1;

    /// The file this client's pins live in.
    #[must_use]
    pub fn path(app_id: &str) -> PathBuf {
        store::path(app_id, "pins.json")
    }
}

impl Default for Pins {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            ids: Vec::new(),
        }
    }
}

/// Reads a client's pins.
///
/// Missing, unreadable or newer than this build means no pins: an ordering
/// preference must never keep the window from opening.
#[must_use]
pub fn load_pins(app_id: &str) -> Pins {
    let path = Pins::path(app_id);
    match store::read::<Pins>(&path) {
        Ok(Some(pins)) if pins.version <= Pins::VERSION => pins,
        Ok(Some(pins)) => {
            tracing::info!(
                target: "settings",
                "ignoring pins version {} at {path:?}",
                pins.version
            );
            Pins::default()
        }
        Ok(None) => Pins::default(),
        Err(error) => {
            tracing::info!(target: "settings", "starting without pins: {error}");
            Pins::default()
        }
    }
}

/// Writes a client's pins, replacing the file.
pub fn save_pins(app_id: &str, pins: &Pins) -> Result<(), store::StoreError> {
    store::write(&Pins::path(app_id), pins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_the_newer_fields_keeps_the_defaults() {
        let settings: UiSettings = sonic_rs::from_slice(br#"{"tearing":true}"#).expect("parse");
        assert_eq!(settings.version, UiSettings::VERSION);
        assert_eq!(settings.hdr, HdrMode::Auto);
        assert!(settings.tearing);
    }

    #[test]
    fn a_round_trip_keeps_every_field() {
        let written = UiSettings {
            version: UiSettings::VERSION,
            hdr: HdrMode::Auto,
            tearing: true,
        };
        let json = sonic_rs::to_string(&written).expect("serialise");
        let read_back: UiSettings = sonic_rs::from_slice(json.as_bytes()).expect("parse");
        assert_eq!(read_back, written);
    }

    #[test]
    fn pins_without_a_version_are_v1_with_no_ids() {
        let pins: Pins = sonic_rs::from_slice(br#"{}"#).expect("parse");
        assert_eq!(pins.version, Pins::VERSION);
        assert!(pins.ids.is_empty());
    }
}
