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
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default, rename = "channel")]
    pub channels: Vec<ChannelConfig>,
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
    /// Base64 secret for a private channel; hashtag channels derive theirs
    /// from the name.
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
        assert_eq!(config.mqtt.port, 1883);
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
