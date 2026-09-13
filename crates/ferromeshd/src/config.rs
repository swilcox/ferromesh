//! `ferromesh.toml`; see `ferromesh.example.toml` for an annotated copy.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ferromesh_model::DEFAULT_PORT;
use serde::Deserialize;

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
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self { listen: default_listen() }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    /// Base64 secret for a private channel; hashtag channels derive theirs
    /// from the name.
    pub key: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
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

    #[test]
    fn example_config_parses() {
        let example = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ferromesh.example.toml");
        let config = Config::load(Path::new(example)).unwrap();
        assert_eq!(config.mqtt.port, 1883);
        assert_eq!(config.api.listen.port(), DEFAULT_PORT);
        assert!(config.channels.iter().all(|channel| channel.key.is_none()));
        assert!(!config.channels.is_empty());
    }

    #[test]
    fn api_section_is_optional() {
        let config: Config =
            toml::from_str("data_dir = \"/data\"\n[mqtt]\nhost = \"127.0.0.1\"\n").unwrap();
        assert_eq!(config.api.listen, default_listen());
    }
}
