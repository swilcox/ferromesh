//! `ferromesh channels`: list, add, unknown and guess.

use std::io::{self, Write};

use anyhow::Result;
use ferromesh_model::{
    AddChannel, ChannelAdded, ChannelInfo, GuessChannels, GuessReport, UnknownChannel,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::Serialize;

use crate::render;
use crate::server::Server;

pub async fn list(server: &Server, json: bool) -> Result<()> {
    let channels: Vec<ChannelInfo> = server.get("/api/v1/channels").await?;
    if json {
        return json_lines(&channels);
    }
    let rows: Vec<Vec<String>> = channels
        .iter()
        .map(|channel| {
            vec![
                channel.name.clone(),
                channel.kind.clone(),
                format!("{:02x}", channel.hash),
                channel.messages.to_string(),
                channel.last_message_at.map(local).unwrap_or_default(),
            ]
        })
        .collect();
    print(&table(&["CHANNEL", "KIND", "HASH", "MESSAGES", "LAST MESSAGE"], &rows, &[3]))
}

pub async fn add(
    server: &Server,
    name: String,
    key: Option<String>,
    token: Option<&str>,
) -> Result<()> {
    let added: ChannelAdded =
        server.post("/api/v1/channels", &AddChannel { name, key }, token).await?;
    let (channel, backfill) = (&added.channel, added.backfill);
    print(&format!(
        "added {} ({}, hash {:02x}): decrypted {} of {} waiting packets, {} messages\n",
        channel.name,
        channel.kind,
        channel.hash,
        backfill.decrypted,
        backfill.checked,
        backfill.messages
    ))
}

pub async fn unknown(server: &Server, json: bool) -> Result<()> {
    let unknown: Vec<UnknownChannel> = server.get("/api/v1/channels/unknown").await?;
    if json {
        return json_lines(&unknown);
    }
    if unknown.is_empty() {
        render::status("every stored channel packet opens with a known key");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = unknown
        .iter()
        .map(|channel| {
            vec![
                format!("{:02x}", channel.hash),
                channel.packets.to_string(),
                channel.heard.to_string(),
                channel.text_packets.to_string(),
                channel.data_packets.to_string(),
                local(channel.first_seen_at),
                local(channel.last_seen_at),
                channel.shares_hash_with.clone().unwrap_or_default(),
            ]
        })
        .collect();
    let header =
        ["HASH", "PACKETS", "HEARD", "TEXT", "DATA", "FIRST SEEN", "LAST SEEN", "SHARES HASH WITH"];
    print(&table(&header, &rows, &[1, 2, 3, 4]))?;
    render::status("try names with: ferromesh channels guess NAME...");
    Ok(())
}

pub async fn guess(
    server: &Server,
    request: GuessChannels,
    add_hits: bool,
    token: Option<&str>,
) -> Result<()> {
    let report: GuessReport = server.post("/api/v1/channels/guess", &request, None).await?;
    print(&format!("tried {} names; {} open stored traffic\n", report.tried, report.hits.len()))?;
    if report.hits.is_empty() {
        return Ok(());
    }
    let rows: Vec<Vec<String>> = report
        .hits
        .iter()
        .map(|hit| {
            vec![
                hit.name.clone(),
                format!("{:02x}", hit.hash),
                hit.packets.to_string(),
                hit.messages.to_string(),
            ]
        })
        .collect();
    print(&table(&["CHANNEL", "HASH", "PACKETS", "MESSAGES"], &rows, &[2, 3]))?;
    if add_hits {
        for hit in report.hits {
            add(server, hit.name, None, token).await?;
        }
    } else {
        render::status("keep one with: ferromesh channels add '#name'");
    }
    Ok(())
}

fn local(at: Timestamp) -> String {
    at.to_zoned(TimeZone::system()).strftime("%Y-%m-%d %H:%M").to_string()
}

/// Columns padded to their widest cell; those listed in `right` align right.
fn table(header: &[&str], rows: &[Vec<String>], right: &[usize]) -> String {
    let rows: Vec<Vec<&str>> = std::iter::once(header.to_vec())
        .chain(rows.iter().map(|row| row.iter().map(String::as_str).collect()))
        .collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|column| rows.iter().map(|row| row[column].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in &rows {
        let cells: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(column, cell)| {
                let pad = " ".repeat(widths[column] - cell.chars().count());
                if right.contains(&column) {
                    format!("{pad}{cell}")
                } else {
                    format!("{cell}{pad}")
                }
            })
            .collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn print(text: &str) -> Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()?;
    Ok(())
}

fn json_lines<T: Serialize>(items: &[T]) -> Result<()> {
    let mut out = io::stdout().lock();
    for item in items {
        writeln!(out, "{}", serde_json::to_string(item)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_align() {
        let rows = vec![
            vec!["#test".to_owned(), "1203".to_owned()],
            vec!["#chattanooga".to_owned(), "37".to_owned()],
        ];
        let expected = format!(
            "CHANNEL{}MESSAGES\n#test{}1203\n#chattanooga{}37\n",
            " ".repeat(7),
            " ".repeat(13),
            " ".repeat(8)
        );
        assert_eq!(table(&["CHANNEL", "MESSAGES"], &rows, &[1]), expected);
    }
}
