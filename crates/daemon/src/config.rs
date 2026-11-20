//! Persisted configuration: Siphon's JSON schema, migrated to the current
//! version on load and written asynchronously on the work thread.
//!
//! The type itself lives in `siphon-shared` because the UI receives it in the
//! snapshot; this module owns the file.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use siphon_shared::rpc::Channel;
use siphon_shared::rpc::Config;
use siphon_shared::store;
use siphon_shared::store::StoreError;
use thiserror::Error;

/// Config directory name. Debug builds use a separate id so a debug daemon
/// runs alongside the installed app without sharing channels.
#[cfg(debug_assertions)]
pub const APP_ID: &str = "com.iken.siphon.debug";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "com.iken.siphon";

/// The one default in the schema: files older than v2 gain word filtering.
const DEFAULT_FILTERED_WORD: &str = "offline";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{0}")]
    Invalid(String),
}

/// Mirror used only while migrating: every field is optional so a missing
/// field can be told apart from an empty list (a cleared filter list is data).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct RawConfig {
    version: Option<u64>,
    channels: Option<Vec<Channel>>,
    notify_title_changes: Option<bool>,
    sound: Option<bool>,
    filtered_words: Option<Vec<String>>,
}

/// The config path under the platform config directory.
#[must_use]
pub fn default_path(app_id: &str) -> PathBuf {
    store::path(app_id, "config.json")
}

/// Loads and migrates a config. Any failure starts from a fresh default: a
/// corrupt file must never keep the daemon from running.
#[must_use]
pub fn load(path: &Path) -> Config {
    match load_versioned(path) {
        Ok(config) => config,
        Err(error) => {
            tracing::info!(target: "config", "starting with fresh config: {error}");
            Config::default()
        }
    }
}

fn load_versioned(path: &Path) -> Result<Config, ConfigError> {
    let mut raw: RawConfig = store::read(path)?
        .ok_or_else(|| ConfigError::Invalid(format!("no config at {}", path.display())))?;
    let from = raw.version.unwrap_or(0);
    if from > Config::VERSION {
        return Err(ConfigError::Invalid(format!(
            "ignoring config version {from}"
        )));
    }
    // v1 files predate word filtering: add the default only when the file
    // never had the field. An explicitly cleared list stays empty.
    if from < 2 && raw.filtered_words.is_none() {
        raw.filtered_words = Some(vec![DEFAULT_FILTERED_WORD.to_owned()]);
    }
    // v2 -> v3: `Channel::record` is missing from older files and serde
    // defaults it to false (notify-only). v3 -> v4: `Channel::notifyLive` and
    // `notifyTitles` are missing and `Channel::default` has both on, so an
    // existing follow keeps the behaviour the global switches gave it.
    let config = Config {
        version: Config::VERSION,
        channels: raw.channels.unwrap_or_default(),
        notify_title_changes: raw.notify_title_changes.unwrap_or(true),
        sound: raw.sound.unwrap_or(true),
        filtered_words: raw
            .filtered_words
            .unwrap_or_else(|| vec![DEFAULT_FILTERED_WORD.to_owned()]),
    };
    if from < Config::VERSION {
        tracing::info!(
            target: "config",
            "migrated config version {from} to {}",
            Config::VERSION
        );
    }
    Ok(config)
}

/// Writes the config atomically enough for this app: one async write on the
/// work thread, never on the reactor's blocking path.
pub async fn save(config: &Config, path: &Path) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        compio::fs::create_dir_all(parent)
            .await
            .map_err(ConfigError::Io)?;
    }
    let json = store::to_json(config)?;
    compio::fs::write(path, json)
        .await
        .0
        .map_err(ConfigError::Io)?;
    Ok(())
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;

    fn scratch_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("siphon-config-{name}-{}.json", std::process::id()))
    }

    #[test]
    fn load_migrates_older_versions_preserving_channels() {
        for (name, version) in [("v1", 1), ("unversioned", 0)] {
            let path = scratch_path(name);
            std::fs::write(
                &path,
                format!(r#"{{"version":{version},"channels":[{{"login":"alice","id":7}}]}}"#),
            )
            .unwrap();
            let config = load(&path);
            assert_eq!(config.version, Config::VERSION, "{name}");
            assert_eq!(
                config
                    .channels
                    .iter()
                    .map(|channel| channel.login.as_str())
                    .collect::<Vec<_>>(),
                ["alice"],
                "{name}"
            );
            assert_eq!(config.filtered_words, ["offline"], "{name}");
            assert!(!config.channels[0].record, "{name}");
            assert!(config.channels[0].notify_live, "{name}");
            assert!(config.channels[0].notify_titles, "{name}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn load_discards_newer_version_for_fresh_default() {
        let path = scratch_path("newer");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":{},"channels":[{{"login":"alice","id":7}}]}}"#,
                Config::VERSION + 1
            ),
        )
        .unwrap();
        let config = load(&path);
        assert_eq!(config.version, Config::VERSION);
        assert!(config.channels.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn current_version_keeps_an_explicitly_cleared_filter_list() {
        let path = scratch_path("cleared");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":{},"channels":[],"filteredWords":[]}}"#,
                Config::VERSION
            ),
        )
        .unwrap();
        let config = load(&path);
        assert_eq!(config.filtered_words, Vec::<String>::new());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn v2_channels_default_to_notify_only_and_keep_record_when_present() {
        let path = scratch_path("record");
        std::fs::write(
            &path,
            r#"{"version":2,"channels":[
                {"login":"alice","id":7},
                {"login":"bob","id":8,"record":true}
            ]}"#,
        )
        .unwrap();
        let config = load(&path);
        assert!(!config.channels[0].record);
        assert!(config.channels[1].record);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_round_trips_through_the_migrating_loader() {
        let path = scratch_path("roundtrip");
        let config = Config {
            channels: vec![Channel {
                login: "alice".to_owned(),
                id: 7,
                display_name: Some("Alice".to_owned()),
                record: true,
                ..Channel::default()
            }],
            filtered_words: vec!["spoiler".to_owned()],
            sound: false,
            ..Config::default()
        };
        compio::runtime::Runtime::new().unwrap().block_on(async {
            save(&config, &path).await.unwrap();
        });
        let loaded = load(&path);
        assert_eq!(loaded.channels.len(), 1);
        assert!(loaded.channels[0].record);
        assert_eq!(loaded.filtered_words, ["spoiler"]);
        assert!(!loaded.sound);
        std::fs::remove_file(&path).ok();
    }
}
