//! Client settings, in `$FERROMESH_CONFIG_DIR`, `$XDG_CONFIG_HOME/ferromesh`
//! or `~/.config/ferromesh`:
//!
//! - `config.toml`, written by you: the default `server` and `token`.
//! - `watches.toml`, kept by the TUI: saved watches.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ferromesh_model::Kind;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Used when neither `--server` nor `FERROMESH_SERVER` is given.
    pub server: Option<String>,
    /// Used when neither `--token` nor `FERROMESH_TOKEN` is given.
    pub token: Option<String>,
}

/// A saved filter the TUI highlights and alerts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConfig {
    pub name: String,
    #[serde(default = "messages")]
    pub kind: Kind,
    pub filter: String,
}

fn messages() -> Kind {
    Kind::Messages
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WatchFile {
    #[serde(default, rename = "watch")]
    watches: Vec<WatchConfig>,
}

pub fn dir() -> Option<PathBuf> {
    let env = |name| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
    env("FERROMESH_CONFIG_DIR")
        .or_else(|| env("XDG_CONFIG_HOME").map(|dir| dir.join("ferromesh")))
        .or_else(|| env("HOME").map(|home| home.join(".config").join("ferromesh")))
}

pub fn load_settings(dir: &Path) -> Result<Settings> {
    read_toml(&dir.join("config.toml"))
}

pub fn load_watches(dir: &Path) -> Result<Vec<WatchConfig>> {
    Ok(read_toml::<WatchFile>(&dir.join("watches.toml"))?.watches)
}

pub fn save_watches(dir: &Path, watches: &[WatchConfig]) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = WatchFile { watches: watches.to_vec() };
    let text = format!(
        "# Kept by `ferromesh tui`. Edit it while the TUI isn't running.\n\n{}",
        toml::to_string(&file)?
    );
    let path = dir.join("watches.toml");
    let partial = dir.join("watches.toml.partial");
    fs::write(&partial, text).with_context(|| format!("writing {}", partial.display()))?;
    fs::rename(&partial, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// A missing file reads as the default.
fn read_toml<T: Default + DeserializeOwned>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_files_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_settings(dir.path()).unwrap(), Settings::default());
        assert!(load_watches(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn watches_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("ferromesh");
        let watches = vec![
            WatchConfig { name: "bot".into(), kind: Kind::Messages, filter: "from:BNABot".into() },
            WatchConfig { name: "strong".into(), kind: Kind::Observations, filter: "snr>5".into() },
        ];
        save_watches(&nested, &watches).unwrap();
        assert_eq!(load_watches(&nested).unwrap(), watches);
    }

    #[test]
    fn settings_parse() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.toml"), "server = \"truffles.local\"\n").unwrap();
        let settings = load_settings(dir.path()).unwrap();
        assert_eq!(settings.server.as_deref(), Some("truffles.local"));
        assert_eq!(settings.token, None);

        fs::write(dir.path().join("watches.toml"), "[[watch]]\nname = \"x\"\nfilter = \"storm\"\n")
            .unwrap();
        assert_eq!(load_watches(dir.path()).unwrap()[0].kind, Kind::Messages);
    }
}
