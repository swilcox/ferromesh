//! `ferromesh dms`: direct messages sent to your companion radio.

use anyhow::Result;
use ferromesh_model::{DirectMessageInfo, DirectQuery};

use crate::channels::{json_lines, local, print, table};
use crate::render;
use crate::server::Server;

pub async fn list(server: &Server, limit: usize, json: bool) -> Result<()> {
    let path = server.path("/api/v1/direct", &DirectQuery { limit: Some(limit) })?;
    let mut messages: Vec<DirectMessageInfo> = server.get(&path).await?;
    messages.reverse();
    if json {
        return json_lines(&messages);
    }
    if messages.is_empty() {
        render::status("no direct messages yet");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = messages.iter().map(row).collect();
    print(&table(&["RECEIVED", "FROM", "KEY", "HOPS", "SNR", "MESSAGE"], &rows, &[3, 4]))
}

fn row(message: &DirectMessageInfo) -> Vec<String> {
    vec![
        local(message.received_at),
        message.sender.clone().unwrap_or_else(|| "?".into()),
        message.sender_prefix.clone(),
        message.hops.map_or_else(|| "direct".into(), |hops| hops.to_string()),
        message.snr.map(|snr| format!("{snr:.1}")).unwrap_or_default(),
        message.body.clone(),
    ]
}
