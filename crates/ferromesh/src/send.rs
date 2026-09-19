//! `ferromesh send`: send through the server's companion radio, then follow
//! what happens to the message.

use std::time::Duration;

use anyhow::{Context, Result};
use ferromesh_model::{OutboxQuery, SendRequest, SendStatus, SentMessageInfo};

use crate::channels::local;
use crate::server::Server;

const CHECK_EVERY: Duration = Duration::from_secs(1);

pub async fn send(
    server: &Server,
    to: String,
    text: String,
    token: Option<&str>,
    follow: Duration,
) -> Result<()> {
    let sent: SentMessageInfo =
        server.post("/api/v1/send", &SendRequest { to, text }, token).await?;
    println!("sent to {} as {} at {}", sent.to, sent.from, local(sent.sent_at));
    let mut last = progress(&sent);
    println!("  {last}");

    let path = server.path("/api/v1/outbox", &OutboxQuery { limit: Some(100) })?;
    let deadline = tokio::time::Instant::now() + follow;
    // A direct message's outcome is final once known; a channel message can
    // keep being heard, so it's followed until the time runs out.
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(CHECK_EVERY).await;
        let outbox: Vec<SentMessageInfo> = server.get(&path).await?;
        let now = outbox
            .into_iter()
            .find(|entry| entry.id == sent.id)
            .context("the message left the outbox")?;
        let line = progress(&now);
        if line != last {
            println!("  {line}");
            last = line;
        }
        if matches!(
            now.status,
            SendStatus::Delivered | SendStatus::Unacknowledged | SendStatus::Failed
        ) {
            break;
        }
    }
    Ok(())
}

fn progress(sent: &SentMessageInfo) -> String {
    match sent.status {
        SendStatus::Failed => {
            format!("failed: {}", sent.error.as_deref().unwrap_or("unknown error"))
        }
        SendStatus::Delivered => match sent.round_trip_ms {
            Some(ms) => format!("delivered, acknowledged in {:.1} s", f64::from(ms) / 1000.0),
            None => "delivered".into(),
        },
        SendStatus::Unacknowledged => "no acknowledgement yet; it may not have arrived".into(),
        SendStatus::Heard => {
            let times =
                if sent.heard == 1 { "once".into() } else { format!("{} times", sent.heard) };
            format!("heard {times} by {}", sent.heard_by.join(", "))
        }
        SendStatus::Sent if sent.direct => "transmitted; waiting for the acknowledgement".into(),
        SendStatus::Sent => "transmitted; waiting for an observer to hear it".into(),
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;

    use super::*;

    fn entry(status: SendStatus, direct: bool) -> SentMessageInfo {
        SentMessageInfo {
            id: 1,
            sent_at: Timestamp::UNIX_EPOCH,
            from: "scw".into(),
            to: "#test".into(),
            direct,
            body: "hi".into(),
            sender_timestamp: Timestamp::UNIX_EPOCH,
            status,
            error: None,
            round_trip_ms: Some(1800),
            heard: 3,
            heard_by: vec!["Tanyard".into(), "scw".into()],
            packet_hash: None,
        }
    }

    #[test]
    fn progress_lines() {
        assert_eq!(progress(&entry(SendStatus::Heard, false)), "heard 3 times by Tanyard, scw");
        assert_eq!(
            progress(&entry(SendStatus::Delivered, true)),
            "delivered, acknowledged in 1.8 s"
        );
        assert_eq!(
            progress(&entry(SendStatus::Sent, true)),
            "transmitted; waiting for the acknowledgement"
        );
    }
}
