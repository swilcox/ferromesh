//! Keeping the radio's contact list to the nodes you talk to.
//!
//! The radio adds chat radios as it hears them and, once full, replaces the
//! contact it heard from least recently, unless that contact is a favourite
//! (see [`Session::apply_contact_policy`]). On top of that:
//!
//! - Everyone you exchange direct messages with becomes a favourite.
//! - A direct message the radio couldn't read, because it had never heard or
//!   had forgotten the sender, is rescued: the possible senders are added
//!   from the server's directory, so the sender's automatic retry can be read.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use ferromesh_store::{NodeContact, Reader};
use meshcore_proto::companion::Contact;
use meshcore_proto::{Packet, Payload, PayloadType};
use tracing::{info, warn};

use super::session::Session;

/// How long a direct message may go unread before rescuing its sender. The
/// radio hands over what it can read within a moment.
const RESCUE_AFTER: Duration = Duration::from_secs(2);
/// Senders retry a few times within a minute; rescue once for all of them.
const RESCUE_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// Advert types that send direct messages: chat radios and room servers.
const SENDER_KINDS: [u8; 2] = [1, 3];

/// Nodes the server has heard advertise.
pub trait Directory {
    /// Nodes whose key starts with `prefix`, most recently heard first.
    fn nodes_by_key_prefix(&self, prefix: &[u8]) -> Vec<NodeContact>;
}

/// The server's database.
pub struct Database(pub PathBuf);

impl Directory for Database {
    fn nodes_by_key_prefix(&self, prefix: &[u8]) -> Vec<NodeContact> {
        Reader::open(&self.0).and_then(|reader| reader.nodes_by_key_prefix(prefix)).unwrap_or_else(
            |error| {
                warn!("couldn't look up nodes: {error}");
                Vec::new()
            },
        )
    }
}

/// A node as the radio's contact, from its newest advert. With no known
/// route, the radio floods to it.
pub fn contact_from(node: &NodeContact) -> Contact {
    Contact {
        pubkey: node.pubkey,
        kind: node.role,
        flags: 0,
        out_path_len: None,
        out_path: Vec::new(),
        name: node.name.clone().unwrap_or_default(),
        last_advert: node.adv_timestamp,
        lat_e6: node.lat_e6.unwrap_or(0),
        lon_e6: node.lon_e6.unwrap_or(0),
    }
}

/// Watches for direct messages to the radio that it doesn't hand over.
#[derive(Debug, Default)]
pub struct Rescue {
    /// Senders' key bytes, with when their message was heard.
    waiting: Vec<(u8, Instant)>,
    rescued: HashMap<u8, Instant>,
}

impl Rescue {
    /// Notes a packet the radio heard, if it's a direct message to `radio`.
    pub fn heard(&mut self, raw: &[u8], radio: u8, now: Instant) {
        let Ok(packet) = Packet::parse(raw) else { return };
        let Ok(Payload::Addressed(message)) = packet.decode_payload() else { return };
        if message.kind == PayloadType::TxtMsg
            && message.dest_hash == radio
            && !self.waiting.iter().any(|(sender, _)| *sender == message.src_hash)
        {
            self.waiting.push((message.src_hash, now));
        }
    }

    /// The radio handed over a message from this sender, so it could read it.
    pub fn delivered(&mut self, sender: u8) {
        self.waiting.retain(|(waiting, _)| *waiting != sender);
    }

    /// Senders whose message is overdue and who weren't rescued recently.
    pub fn due(&mut self, now: Instant) -> Vec<u8> {
        let mut due = Vec::new();
        self.waiting.retain(|&(sender, heard)| {
            if now.duration_since(heard) < RESCUE_AFTER {
                return true;
            }
            let recent = self
                .rescued
                .get(&sender)
                .is_some_and(|at| now.duration_since(*at) < RESCUE_AGAIN_AFTER);
            if !recent {
                due.push(sender);
            }
            false
        });
        for sender in &due {
            self.rescued.insert(*sender, now);
        }
        due
    }
}

/// Adds the nodes that could have sent an unreadable direct message. Does
/// nothing if one of them is already a contact: the radio could read it.
pub fn rescue<L: Read + Write>(
    session: &mut Session<L>,
    directory: &dyn Directory,
    sender: u8,
) -> Result<()> {
    let candidates: Vec<Contact> = directory
        .nodes_by_key_prefix(&[sender])
        .iter()
        .filter(|node| SENDER_KINDS.contains(&node.role))
        .map(contact_from)
        .collect();
    if candidates.is_empty() {
        warn!(
            sender = format!("{sender:02x}"),
            "a direct message to the companion radio couldn't be read: its sender has never been heard advertising"
        );
        return Ok(());
    }
    for candidate in &candidates {
        if let Ok(Some(_)) = session.get_contact(&candidate.pubkey)? {
            return Ok(());
        }
    }
    let mut added = Vec::new();
    for candidate in &candidates {
        match session.add_contact(candidate)? {
            Ok(true) => added.push(candidate.name.clone()),
            Ok(false) => {}
            Err(refusal) => warn!("couldn't add a possible sender: {refusal}"),
        }
    }
    info!(
        senders = %added.join(", "),
        "a direct message to the companion radio couldn't be read; added its possible senders so a retry can be"
    );
    Ok(())
}

/// Makes the sender of a direct message a favourite, so the radio keeps it.
pub fn pin_sender<L: Read + Write>(
    session: &mut Session<L>,
    directory: &dyn Directory,
    prefix: &[u8; 6],
) -> Result<()> {
    let known = directory.nodes_by_key_prefix(prefix);
    let contact = match known.as_slice() {
        [node] => contact_from(node),
        // Not in the directory, but the radio read the message, so it has
        // the sender.
        _ => match session.list_contacts()?.into_iter().find(|c| c.pubkey[..6] == prefix[..]) {
            Some(contact) => contact,
            None => return Ok(()),
        },
    };
    if let Ok(Some(held)) = session.get_contact(&contact.pubkey)?
        && held.is_favourite()
    {
        return Ok(());
    }
    match session.set_favourite(&contact, true)? {
        Ok(held) => {
            info!(contact = %held.name, "kept a direct-message sender as a favourite contact")
        }
        Err(refusal) => warn!("couldn't keep a direct-message sender: {refusal}"),
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::session::tests::FakeRadio;
    use super::*;

    pub(crate) struct Nodes(pub Vec<NodeContact>);

    impl Directory for Nodes {
        fn nodes_by_key_prefix(&self, prefix: &[u8]) -> Vec<NodeContact> {
            self.0.iter().filter(|node| node.pubkey.starts_with(prefix)).cloned().collect()
        }
    }

    fn node(first: u8, second: u8, name: &str, role: u8) -> NodeContact {
        let mut pubkey = [second; 32];
        pubkey[0] = first;
        NodeContact {
            pubkey,
            name: Some(name.into()),
            role,
            adv_timestamp: 1,
            lat_e6: None,
            lon_e6: None,
        }
    }

    /// A flood direct message from `src` to `dest`.
    fn txt_msg(dest: u8, src: u8) -> Vec<u8> {
        [&[0x09, 0x00, dest, src, 0x12, 0x34][..], &[0; 16]].concat()
    }

    #[test]
    fn unread_messages_are_rescued_once() {
        let mut rescue = Rescue::default();
        let t0 = Instant::now();
        rescue.heard(&txt_msg(0xF9, 0xD2), 0xF9, t0);
        rescue.heard(&txt_msg(0x11, 0xD2), 0xF9, t0); // to someone else
        rescue.heard(&txt_msg(0xF9, 0xD2), 0xF9, t0); // a retry
        assert!(rescue.due(t0 + Duration::from_secs(1)).is_empty(), "too soon");
        assert_eq!(rescue.due(t0 + Duration::from_secs(3)), [0xD2]);

        // The next retry is heard, but the sender was just rescued.
        rescue.heard(&txt_msg(0xF9, 0xD2), 0xF9, t0 + Duration::from_secs(10));
        assert!(rescue.due(t0 + Duration::from_secs(13)).is_empty());

        // A message the radio hands over isn't rescued.
        rescue.heard(&txt_msg(0xF9, 0x07), 0xF9, t0);
        rescue.delivered(0x07);
        assert!(rescue.due(t0 + Duration::from_secs(90)).is_empty());
    }

    #[test]
    fn rescue_adds_possible_senders_unless_one_is_known() {
        let nodes = Nodes(vec![
            node(0xD2, 1, "KK4SW", 1),
            node(0xD2, 2, "Other person", 1),
            node(0xD2, 3, "A repeater", 2),
            node(0x99, 4, "Unrelated", 1),
        ]);
        let mut session = Session::start(FakeRadio::default()).unwrap();
        rescue(&mut session, &nodes, 0xD2).unwrap();
        let names: Vec<String> =
            session.list_contacts().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["KK4SW", "Other person"]);

        // With a possible sender already a contact, nothing more is added.
        let nodes = Nodes(vec![node(0xD2, 1, "KK4SW", 1), node(0xD2, 5, "Newcomer", 1)]);
        rescue(&mut session, &nodes, 0xD2).unwrap();
        assert_eq!(session.list_contacts().unwrap().len(), 2);
    }

    #[test]
    fn direct_message_senders_become_favourites() {
        let nodes = Nodes(vec![node(0xD2, 1, "KK4SW", 1)]);
        let mut session = Session::start(FakeRadio::default()).unwrap();
        let prefix: [u8; 6] = nodes.0[0].pubkey[..6].try_into().unwrap();
        pin_sender(&mut session, &nodes, &prefix).unwrap();
        let contacts = session.list_contacts().unwrap();
        assert_eq!((contacts[0].name.as_str(), contacts[0].is_favourite()), ("KK4SW", true));

        // Unknown to the directory and the radio: nothing to pin.
        pin_sender(&mut session, &Nodes(Vec::new()), &[7; 6]).unwrap();
        assert_eq!(session.list_contacts().unwrap().len(), 1);
    }
}
