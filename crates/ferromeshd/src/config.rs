//! `ferromesh.toml`; see `ferromesh.example.toml` for an annotated copy.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ferromesh_model::DEFAULT_PORT;
use serde::Deserialize;

/// Shorter tokens are too easy to guess over the LAN.
const MIN_TOKEN_LEN: usize = 16;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Holds `ferromesh.db` and the `raw/` log.
    pub data_dir: PathBuf,
    /// A broker publishing what observer repeaters hear; none without this
    /// section, leaving the companion radio as the only source.
    pub mqtt: Option<MqttConfig>,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default, rename = "channel")]
    pub channels: Vec<ChannelConfig>,
    /// A companion radio to record from; none without this section.
    pub companion: Option<CompanionConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompanionConfig {
    /// The radio's serial port, or `auto` for the one Espressif USB device.
    #[serde(default = "default_device")]
    pub device: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Must be unique per running instance, or the broker disconnects one.
    #[serde(default = "default_client_id")]
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_topics")]
    pub topics: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    /// Where the HTTP and WebSocket API listens.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The bearer token clients need to change anything, such as adding a
    /// channel. Without one the API is read-only.
    pub token: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { listen: default_listen(), token: None }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    /// `#name` for a hashtag channel; any name for a channel with a `key`.
    pub name: String,
    /// A private channel's secret, in hex or base64; hashtag channels derive
    /// theirs from the name.
    pub key: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        ensure!(
            config.mqtt.is_some() || config.companion.is_some(),
            "nothing to record: give a [mqtt] broker, a [companion] radio, or both"
        );
        if let Some(token) = &config.api.token {
            ensure!(
                token.len() >= MIN_TOKEN_LEN,
                "api.token must be at least {MIN_TOKEN_LEN} characters"
            );
        }
        Ok(config)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("ferromesh.db")
    }

    pub fn raw_dir(&self) -> PathBuf {
        self.data_dir.join("raw")
    }
}

fn default_port() -> u16 {
    1883
}

fn default_client_id() -> String {
    "ferromeshd".to_owned()
}

fn default_topics() -> Vec<String> {
    vec!["meshcore/+/+/packets".to_owned(), "meshcore/+/+/status".to_owned()]
}

fn default_device() -> String {
    "auto".to_owned()
}

fn default_listen() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "data_dir = \"/data\"\n[mqtt]\nhost = \"127.0.0.1\"\n";

    #[test]
    fn example_config_parses() {
        let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ferromesh.example.toml");
        let config = Config::load(Path::new(example)).unwrap();
        assert_eq!(config.mqtt.unwrap().port, 1883);
        assert_eq!(config.api.listen.port(), DEFAULT_PORT);
        assert_eq!(config.api.token, None);
        assert!(config.channels.iter().all(|channel| channel.key.is_none()));
        assert!(!config.channels.is_empty());
    }

    #[test]
    fn api_section_is_optional() {
        let config = Config::parse(MINIMAL).unwrap();
        assert_eq!(config.api.listen, default_listen());
        assert_eq!(config.api.token, None);
    }

    #[test]
    fn a_radio_alone_is_enough() {
        let radio_only = "data_dir = \"/data\"\n[companion]\n";
        let config = Config::parse(radio_only).unwrap();
        assert!(config.mqtt.is_none());
        assert!(config.companion.is_some());
    }

    #[test]
    fn something_must_be_recorded() {
        let error = Config::parse("data_dir = \"/data\"\n").unwrap_err().to_string();
        assert!(error.contains("nothing to record"), "{error}");
    }

    #[test]
    fn companion_section() {
        assert!(Config::parse(MINIMAL).unwrap().companion.is_none());
        let auto = Config::parse(&format!("{MINIMAL}[companion]\n")).unwrap();
        assert_eq!(auto.companion.unwrap().device, "auto");
        let path = Config::parse(&format!("{MINIMAL}[companion]\ndevice = \"/dev/ttyACM0\"\n"));
        assert_eq!(path.unwrap().companion.unwrap().device, "/dev/ttyACM0");
    }

    #[test]
    fn tokens_must_be_long_enough() {
        let with_token =
            |token: &str| Config::parse(&format!("{MINIMAL}[api]\ntoken = \"{token}\"\n"));
        assert!(with_token("short").is_err());
        assert_eq!(
            with_token("a-long-enough-token").unwrap().api.token.as_deref(),
            Some("a-long-enough-token")
        );
    }
}
