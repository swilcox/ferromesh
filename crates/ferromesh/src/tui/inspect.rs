//! Labelling every byte of a frame, for the inspector.

use std::ops::Range;

use jiff::Timestamp;
use meshcore_proto::{Packet, Payload, PayloadType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub label: &'static str,
    pub bytes: Range<usize>,
    pub value: String,
}

/// Labelled byte ranges that cover all of `frame`, in order.
pub fn fields(frame: &[u8]) -> Vec<Field> {
    let packet = match Packet::parse(frame) {
        Ok(packet) => packet,
        Err(error) => {
            return vec![Field {
                label: "unparsed",
                bytes: 0..frame.len(),
                value: error.to_string(),
            }];
        }
    };
    let mut out = Fields { fields: Vec::new(), at: 0, len: frame.len() };
    let header = packet.header();
    out.take(
        "header",
        1,
        format!(
            "{} · {} · v{}",
            header.route_type().name(),
            header.payload_type().name(),
            header.version() + 1
        ),
    );
    if let Some([a, b]) = packet.transport_codes() {
        out.take("transport", 4, format!("{a:04x} {b:04x}"));
    }
    let path = packet.path();
    let what = if packet.payload_type() == PayloadType::Trace { "SNR readings" } else { "hops" };
    let size = path.hash_size();
    out.take(
        "path length",
        1,
        format!(
            "{} {what}, {size} byte{} each",
            path.hop_count(),
            if size == 1 { "" } else { "s" }
        ),
    );
    out.take(
        "path",
        path.as_bytes().len(),
        path.hops().map(hex::encode).collect::<Vec<_>>().join(" "),
    );

    match packet.decode_payload() {
        Ok(Payload::Group(group)) => {
            out.take("channel", 1, format!("{:02x}", group.channel_hash));
            out.take("mac", 2, hex::encode(group.mac));
            out.take("ciphertext", group.ciphertext.len(), bytes(group.ciphertext.len()));
        }
        Ok(Payload::Advert(advert)) => {
            out.take("public key", 32, hex::encode(advert.pubkey));
            let at = Timestamp::from_second(i64::from(advert.timestamp));
            out.take(
                "timestamp",
                4,
                at.map_or_else(|_| advert.timestamp.to_string(), |at| at.to_string()),
            );
            out.take(
                "signature",
                64,
                if advert.verify() { "valid" } else { "does not verify" }.into(),
            );
            match advert.parse_app_data() {
                Ok(app) => {
                    out.take("flags", 1, format!("{:02x}, {}", app.flags, app.role().name()));
                    if let Some(location) = app.location {
                        out.take(
                            "location",
                            8,
                            format!("{:.5}, {:.5}", location.lat(), location.lon()),
                        );
                    }
                    if let Some(feature) = app.feature1 {
                        out.take("feature 1", 2, format!("{feature:04x}"));
                    }
                    if let Some(feature) = app.feature2 {
                        out.take("feature 2", 2, format!("{feature:04x}"));
                    }
                    if let Some(name) = app.name {
                        out.take("name", name.len(), String::from_utf8_lossy(name).into_owned());
                    }
                }
                Err(_) => out.take("app data", advert.app_data.len(), bytes(advert.app_data.len())),
            }
        }
        Ok(Payload::Addressed(message)) => {
            out.take("destination", 1, format!("{:02x}", message.dest_hash));
            out.take("source", 1, format!("{:02x}", message.src_hash));
            out.take("mac", 2, hex::encode(message.mac));
            out.take("ciphertext", message.ciphertext.len(), bytes(message.ciphertext.len()));
        }
        Ok(Payload::AnonReq(request)) => {
            out.take("destination", 1, format!("{:02x}", request.dest_hash));
            out.take("sender key", 32, hex::encode(request.sender_pubkey));
            out.take("mac", 2, hex::encode(request.mac));
            out.take("ciphertext", request.ciphertext.len(), bytes(request.ciphertext.len()));
        }
        Ok(Payload::Ack { checksum }) => out.take("checksum", 4, format!("{checksum:08x}")),
        Ok(Payload::Trace(trace)) => {
            out.take("tag", 4, format!("{:08x}", trace.tag));
            out.take("auth code", 4, format!("{:08x}", trace.auth_code));
            out.take("flags", 1, format!("{:02x}", trace.flags));
            out.take("route", trace.hashes.len(), hex::encode(trace.hashes));
        }
        Ok(Payload::Control(control)) => {
            out.take("flags", 1, format!("{:02x}, sub-type {}", control.flags, control.sub_type()));
            out.take("data", control.data.len(), bytes(control.data.len()));
        }
        Ok(Payload::Opaque(..)) | Err(_) => {}
    }
    out.finish()
}

fn bytes(count: usize) -> String {
    format!("{count} byte{}", if count == 1 { "" } else { "s" })
}

struct Fields {
    fields: Vec<Field>,
    at: usize,
    len: usize,
}

impl Fields {
    fn take(&mut self, label: &'static str, count: usize, value: String) {
        if count == 0 {
            return;
        }
        let end = (self.at + count).min(self.len);
        self.fields.push(Field { label, bytes: self.at..end, value });
        self.at = end;
    }

    /// Labels whatever is left, such as a payload with no known layout.
    fn finish(mut self) -> Vec<Field> {
        let rest = self.len - self.at;
        self.take("payload", rest, bytes(rest));
        self.fields
    }
}

#[cfg(test)]
mod tests {
    use meshcore_proto::{ChannelKey, GroupText};

    use super::*;

    fn labels(fields: &[Field]) -> Vec<&str> {
        fields.iter().map(|field| field.label).collect()
    }

    fn assert_covers(fields: &[Field], len: usize) {
        let mut at = 0;
        for field in fields {
            assert_eq!(field.bytes.start, at, "{fields:?}");
            at = field.bytes.end;
        }
        assert_eq!(at, len);
    }

    #[test]
    fn channel_message() {
        let text =
            GroupText { sender_timestamp: 1, txt_type: 0, attempt: 0, text: b"Bob: hi".to_vec() };
        let frame = [
            &[0x15, 0x42, 0xAB, 0xCD, 0x11, 0x22][..],
            &ChannelKey::from_hashtag("#test").encrypt(&text.to_plaintext()),
        ]
        .concat();
        let fields = fields(&frame);
        assert_eq!(
            labels(&fields),
            ["header", "path length", "path", "channel", "mac", "ciphertext"]
        );
        assert_eq!(fields[0].value, "flood · GRP_TXT · v1");
        assert_eq!(fields[1].value, "2 hops, 2 bytes each");
        assert_eq!(fields[2].value, "abcd 1122");
        assert_covers(&fields, frame.len());
    }

    #[test]
    fn trace_with_transport_codes_style_header() {
        let frame = hex::decode("2601276AF0342431FB3ED301A6A6").unwrap();
        let fields = fields(&frame);
        assert_eq!(
            labels(&fields),
            ["header", "path length", "path", "tag", "auth code", "flags", "route"]
        );
        assert_eq!(fields[1].value, "1 SNR readings, 1 byte each");
        assert_covers(&fields, frame.len());
    }

    #[test]
    fn advert_fields() {
        let mut frame = vec![0x14, 0x34, 0x12, 0x78, 0x56, 0x00];
        frame.extend([0u8; 32]);
        frame.extend(1_789_000_000u32.to_le_bytes());
        frame.extend([0u8; 64]);
        frame.push(0x92);
        frame.extend(36_100_000i32.to_le_bytes());
        frame.extend((-86_800_000i32).to_le_bytes());
        frame.extend(b"Hilltop");
        frame[0] = 0x10; // transport flood ADVERT
        let fields = fields(&frame);
        assert_eq!(
            labels(&fields),
            [
                "header",
                "transport",
                "path length",
                "public key",
                "timestamp",
                "signature",
                "flags",
                "location",
                "name"
            ]
        );
        assert_eq!(fields[6].value, "92, repeater");
        assert_eq!(fields[7].value, "36.10000, -86.80000");
        assert_eq!(fields[8].value, "Hilltop");
        assert_covers(&fields, frame.len());
    }

    #[test]
    fn unparseable_and_truncated_frames() {
        let fields = fields(&[0x09, 0xC1, 0x00]);
        assert_eq!(labels(&fields), ["unparsed"]);
        assert_covers(&fields, 3);

        // An ACK too short for its checksum still covers every byte.
        let short = [0x0D, 0x00, 0x01];
        let fields = super::fields(&short);
        assert_covers(&fields, short.len());
    }
}
