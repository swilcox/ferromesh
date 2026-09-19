//! What a raw record says, whichever source wrote it.

use anyhow::Result;
use ferromesh_store::{DirectMessage, Reception, StatusReport};

use crate::rawlog::RawRecord;
use crate::{companion, meshcoretomqtt};

#[derive(Debug)]
pub enum Message {
    Packet(Reception),
    Status(StatusReport),
    Direct(DirectMessage),
    /// Something we keep in the raw log but don't store, such as an MQTT
    /// `debug` topic.
    Ignored,
}

pub fn parse(record: &RawRecord) -> Result<Message> {
    if record.topic.starts_with(companion::TOPIC_PREFIX) {
        companion::record::parse(record)
    } else {
        meshcoretomqtt::parse(&record.topic, &record.payload)
    }
}
