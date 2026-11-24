//! Persisted configuration: Siphon's JSON schema, migrated to the current
//! version on load and written asynchronously on the work thread.
//!
//! The type itself lives in `siphon-shared` because the UI receives it in the
//! snapshot; this module owns the file.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use siphon_shared::riffle::Account;
use siphon_shared::riffle::Channel;
use siphon_shared::riffle::Config;
use siphon_shared::riffle::Network;
use siphon_shared::store;
use siphon_shared::store::StoreError;
use thiserror::Error;

/// Config directory name. Debug builds use a separate id so a debug daemon
/// runs alongside the installed app without sharing channels.
#[cfg(debug_assertions)]
pub const APP_ID: &str = "com.iken.siphon.debug";
#[cfg(not(debug_assertions))]
pub const APP_ID: &str = "com.iken.siphon";

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
    record_dir: Option<String>,
    record_dual_format: Option<bool>,
    account: Option<Account>,
    network: Option<Network>,
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
    let raw: RawConfig = store::read(path)?
        .ok_or_else(|| ConfigError::Invalid(format!("no config at {}", path.display())))?;
    let from = raw.version.unwrap_or(0);
    if from > Config::VERSION {
        return Err(ConfigError::Invalid(format!(
            "ignoring config version {from}"
        )));
    }
    // v2 -> v3: `Channel::record` is missing from older files and serde
    // defaults it to false. v4 -> v5: `recordDir` and `recordDualFormat` are
    // new; recording stays off until a directory is set, and the second
    // orientation is opt-in. v5 -> v6: the notification switches, the word
    // filter and the per-channel notify switches move to the client's settings
    // file, and `account` and `network` are new. Nothing here reads the old
    // fields: serde ignores what it is not told to keep, and a client's first
    // run after the move starts with the defaults.
    let config = Config {
        version: Config::VERSION,
        channels: raw.channels.unwrap_or_default(),
        record_dir: raw.record_dir,
        record_dual_format: raw.record_dual_format.unwrap_or(false),
        account: raw.account,
        network: raw.network.unwrap_or_default(),
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
            assert!(!config.channels[0].record, "{name}");
            assert!(config.account.is_none(), "{name}");
            assert!(config.network.listen.is_empty(), "{name}");
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
    fn a_v5_file_loses_its_notification_fields_without_losing_channels() {
        let path = scratch_path("v5");
        std::fs::write(
            &path,
            r#"{"version":5,"channels":[{"login":"alice","id":7,"notifyLive":false,
                "notifyTitles":false,"record":true}],
                "notifyTitleChanges":false,"sound":false,"filteredWords":["brb"]}"#,
        )
        .unwrap();
        let config = load(&path);
        assert_eq!(config.version, Config::VERSION);
        assert_eq!(config.channels.len(), 1);
        assert!(config.channels[0].record);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn v2_channels_default_to_not_recording_and_keep_record_when_present() {
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
            }],
            account: Some(Account {
                login: "alice".to_owned(),
                user_id: 7,
            }),
            ..Config::default()
        };
        compio::runtime::Runtime::new().unwrap().block_on(async {
            save(&config, &path).await.unwrap();
        });
        let loaded = load(&path);
        assert_eq!(loaded.channels.len(), 1);
        assert!(loaded.channels[0].record);
        assert_eq!(loaded.account.expect("account").user_id, 7);
        std::fs::remove_file(&path).ok();
    }
}
