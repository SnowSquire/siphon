use std::io::Error as IoError;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// A configured channel. Deserializes both the legacy format (plain
/// `"login"` strings, id not yet resolved) and the current format
/// (`{ "login": .., "id": .., "displayName": .. }`); always serializes as
/// an object so the ui has a stable shape.
#[derive(Debug, Clone)]
pub enum Channel {
    Resolved {
        login: String,
        id: u64,
        display_name: Option<String>,
    },
    Unresolved(String),
}

impl Channel {
    pub fn login(&self) -> &str {
        match self {
            Self::Resolved { login, .. } | Self::Unresolved(login) => login,
        }
    }

    pub const fn id(&self) -> Option<u64> {
        match self {
            Self::Resolved { id, .. } => Some(*id),
            Self::Unresolved(_) => None,
        }
    }

    pub fn display_name(&self) -> Option<&str> {
        match self {
            Self::Resolved { display_name, .. } => display_name.as_deref(),
            Self::Unresolved(_) => None,
        }
    }

    pub fn set_resolved(&mut self, id: u64, display_name: Option<String>) {
        *self = Self::Resolved {
            login: self.login().to_owned(),
            id,
            display_name: display_name.or_else(|| self.display_name().map(String::from)),
        };
    }
}

impl Serialize for Channel {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Repr<'a> {
            login: &'a str,
            id: Option<u64>,
            display_name: Option<&'a str>,
        }
        match self {
            Self::Resolved {
                login,
                id,
                display_name,
            } => Repr {
                login,
                id: Some(*id),
                display_name: display_name.as_deref(),
            }
            .serialize(serializer),
            Self::Unresolved(login) => Repr {
                login,
                id: None,
                display_name: None,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Channel {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Object {
                login: String,
                id: Option<u64>,
                #[serde(default)]
                display_name: Option<String>,
            },
            Login(String),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Object {
                login,
                id: Some(id),
                display_name,
            } => Ok(Self::Resolved {
                login,
                id,
                display_name,
            }),
            Repr::Object { login, .. } | Repr::Login(login) => Ok(Self::Unresolved(login)),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub channels: Vec<Channel>,
    pub notify_title_changes: bool,
    pub sound: bool,
}

impl Config {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), IoError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(IoError::other)?;
        std::fs::write(path, bytes)
    }
}
