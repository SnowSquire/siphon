//! The settings a client keeps for itself.
//!
//! These are per-client, not per-daemon: a display setting belongs to the
//! screen the window is on, and a notification switch belongs to the person
//! looking at it, so two UIs attached to one daemon must be able to differ.
//! That is why they live in their own file instead of the daemon's config,
//! which travels with the broadcast setup rather than with a display.
//!
//! Notification policy lives here too. The daemon publishes transitions
//! (`StreamEvent`); the switches, the word filter and the mute window that
//! decide whether a transition becomes a toast are the client's.

use std::collections::BTreeMap;
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

/// The part of the settings the renderer reads, by value.
///
/// Kept `Copy` so the window can hand it to the renderer without lending the
/// whole document, which is the reason the old settings type was `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderSettings {
    pub hdr: HdrMode,
    pub tearing: bool,
}

/// Notification policy: the global switches, the word filter and the mute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Notifications {
    /// Play a sound with a toast.
    pub sound: bool,
    /// Notify when a followed channel changes its title or game.
    pub title_changes: bool,
    /// A title containing one of these is not notified.
    pub filtered_words: Vec<String>,
    /// How long after a stream ends its title/game notices stay muted: the
    /// churn around the end of a broadcast is noise, going live again is not.
    pub mute_after_stream_down_secs: u64,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            sound: true,
            title_changes: true,
            filtered_words: vec!["offline".to_owned()],
            mute_after_stream_down_secs: 600,
        }
    }
}

/// What this client remembers about one followed channel.
///
/// Keyed by Twitch's channel id, so it survives a rename and means the same
/// thing on every connection. Only a deviation from the defaults is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChannelPrefs {
    /// Notify when this channel goes live.
    pub notify_live: bool,
    /// Notify when its title or game changes.
    pub notify_titles: bool,
    /// Keep it above the sort, whatever the sort says.
    pub pinned: bool,
}

impl Default for ChannelPrefs {
    fn default() -> Self {
        Self {
            notify_live: true,
            notify_titles: true,
            pinned: false,
        }
    }
}

/// One daemon this client can talk to.
///
/// `Local` is this install's pipe or socket. `Remote` is a network daemon;
/// `fingerprint` is the key remembered from enrollment, and the connection is
/// refused when it does not match. `id` is stable across renames and is what
/// `active_connection` names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Connection {
    Local {
        id: String,
        label: String,
    },
    Remote {
        id: String,
        /// `host:port`.
        endpoint: String,
        /// Pinned server key, as printed by the daemon at enrollment.
        fingerprint: String,
        label: String,
    },
}

impl Connection {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Local { id, .. } | Self::Remote { id, .. } => id,
        }
    }

    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Local { label, .. } | Self::Remote { label, .. } => label,
        }
    }

    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local { .. })
    }
}

impl Default for Connection {
    fn default() -> Self {
        Self::Local {
            id: "local".to_owned(),
            label: "this machine".to_owned(),
        }
    }
}

/// What a client persists for itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ClientSettings {
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
    pub notifications: Notifications,
    /// Per-channel preferences, keyed by the channel id as text so the JSON
    /// object keys are what a person would write. Sparse: an entry exists only
    /// where this client deviates from [`ChannelPrefs::default`].
    pub channels: BTreeMap<String, ChannelPrefs>,
    pub connections: Vec<Connection>,
    /// The `id` of the connection in use.
    pub active_connection: String,
}

impl ClientSettings {
    /// Current on-disk schema.
    pub const VERSION: u64 = 2;

    /// The values the renderer needs, without the rest of the document.
    #[must_use]
    pub const fn render(&self) -> RenderSettings {
        RenderSettings {
            hdr: self.hdr,
            tearing: self.tearing,
        }
    }

    /// This client's preferences for one channel, or the defaults.
    #[must_use]
    pub fn channel(&self, id: u64) -> ChannelPrefs {
        self.channels
            .get(&id.to_string())
            .copied()
            .unwrap_or_default()
    }

    /// Writes one channel's preferences, dropping the entry when it has become
    /// the default again so the file does not accumulate no-op rows.
    pub fn set_channel(&mut self, id: u64, prefs: ChannelPrefs) {
        let key = id.to_string();
        if prefs == ChannelPrefs::default() {
            self.channels.remove(&key);
        } else {
            self.channels.insert(key, prefs);
        }
    }

    #[must_use]
    pub fn pinned(&self, id: u64) -> bool {
        self.channel(id).pinned
    }

    /// Flips one channel's pin and returns the new state.
    pub fn toggle_pin(&mut self, id: u64) -> bool {
        let mut prefs = self.channel(id);
        prefs.pinned = !prefs.pinned;
        self.set_channel(id, prefs);
        prefs.pinned
    }

    /// The connection in use, falling back to the local one when the id in the
    /// file no longer names anything.
    #[must_use]
    pub fn active(&self) -> &Connection {
        self.connections
            .iter()
            .find(|connection| connection.id() == self.active_connection)
            .or_else(|| self.connections.first())
            .map_or_else(
                // `connections` is never empty in a default document, but a
                // hand-edited file can be: the local daemon is always there.
                || unreachable_local(),
                |connection| connection,
            )
    }
}

/// A `Connection::Local` for the one case a document cannot supply: an empty
/// `connections` array. Returned by reference, so it has to outlive the call.
fn unreachable_local() -> &'static Connection {
    static LOCAL: std::sync::OnceLock<Connection> = std::sync::OnceLock::new();
    LOCAL.get_or_init(Connection::default)
}

impl Default for ClientSettings {
    fn default() -> Self {
        let local = Connection::default();
        Self {
            version: Self::VERSION,
            hdr: HdrMode::default(),
            tearing: false,
            notifications: Notifications::default(),
            channels: BTreeMap::new(),
            active_connection: local.id().to_owned(),
            connections: vec![local],
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
pub fn load(app_id: &str) -> ClientSettings {
    let path = path(app_id);
    match store::read::<ClientSettings>(&path) {
        Ok(Some(settings)) if settings.version <= ClientSettings::VERSION => settings,
        Ok(Some(settings)) => {
            tracing::info!(
                target: "settings",
                "ignoring settings version {} at {path:?}",
                settings.version
            );
            ClientSettings::default()
        }
        Ok(None) => ClientSettings::default(),
        Err(error) => {
            tracing::info!(target: "settings", "starting with default settings: {error}");
            ClientSettings::default()
        }
    }
}

/// Writes a client's settings, replacing the file.
pub fn save(app_id: &str, settings: &ClientSettings) -> Result<(), store::StoreError> {
    store::write(&path(app_id), settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_the_newer_fields_keeps_the_defaults() {
        let settings: ClientSettings = sonic_rs::from_slice(br#"{"tearing":true}"#).expect("parse");
        assert_eq!(settings.version, ClientSettings::VERSION);
        assert_eq!(settings.hdr, HdrMode::Auto);
        assert!(settings.tearing);
        assert!(settings.notifications.sound);
        assert_eq!(settings.connections.len(), 1);
        assert!(settings.active().is_local());
    }

    #[test]
    fn a_round_trip_keeps_every_field() {
        let mut written = ClientSettings::default();
        written.tearing = true;
        written.notifications.filtered_words = vec!["offline".to_owned(), "brb".to_owned()];
        written.set_channel(
            42,
            ChannelPrefs {
                notify_titles: false,
                pinned: true,
                ..ChannelPrefs::default()
            },
        );
        written.connections.push(Connection::Remote {
            id: "c41f0b2e".to_owned(),
            endpoint: "home.example.net:7418".to_owned(),
            fingerprint: "sha256:4f9c".to_owned(),
            label: "media box".to_owned(),
        });
        written.active_connection = "c41f0b2e".to_owned();
        let json = sonic_rs::to_string(&written).expect("serialise");
        let read_back: ClientSettings = sonic_rs::from_slice(json.as_bytes()).expect("parse");
        assert_eq!(read_back, written);
        assert!(!read_back.active().is_local());
    }

    #[test]
    fn only_a_deviation_is_written() {
        let mut settings = ClientSettings::default();
        settings.set_channel(
            7,
            ChannelPrefs {
                pinned: true,
                ..ChannelPrefs::default()
            },
        );
        assert!(settings.pinned(7));
        assert_eq!(settings.channels.len(), 1);
        assert!(!settings.toggle_pin(7));
        assert!(!settings.pinned(7));
        assert!(settings.channels.is_empty());
    }

    #[test]
    fn a_sparse_channel_entry_takes_the_defaults() {
        let settings: ClientSettings =
            sonic_rs::from_slice(br#"{"channels":{"42":{"pinned":true}}}"#).expect("parse");
        let prefs = settings.channel(42);
        assert!(prefs.pinned);
        assert!(prefs.notify_live);
        assert!(prefs.notify_titles);
        assert!(!settings.pinned(43));
    }

    #[test]
    fn the_render_projection_is_copy() {
        let settings = ClientSettings {
            hdr: HdrMode::On,
            tearing: true,
            ..ClientSettings::default()
        };
        let render = settings.render();
        let copy = render;
        assert_eq!(copy, render);
        assert_eq!(render.hdr, HdrMode::On);
    }
}
