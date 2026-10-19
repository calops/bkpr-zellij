//! Kitty's OSC 5522 clipboard protocol (https://sw.kovidgoyal.net/kitty/clipboard/).
//! Payloads remain encoded: a multiplexer must not reinterpret MIME data or errors.

pub const MAX_PACKET_BYTES: usize = 16 * 1024;
// Enough for the protocol's mandatory 64 MiB of decoded write data, plus framing.
pub const MAX_TRANSFER_BYTES: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet<'a> {
    pub metadata: &'a [u8],
    pub payload: Option<&'a [u8]>,
}

impl<'a> Packet<'a> {
    pub fn parse(payload: &'a [u8]) -> Option<Self> {
        if payload.len() > MAX_PACKET_BYTES { return None; }
        let body = payload.strip_prefix(b"5522;")?;
        let mut parts = body.splitn(2, |b| *b == b';');
        let metadata = parts.next()?;
        if metadata.is_empty() || !metadata.iter().all(|b| (0x20..=0x7e).contains(b)) {
            return None;
        }
        let packet = Self { metadata, payload: parts.next() };
        // Duplicate routing/type fields are ambiguous and must never claim a slot.
        for key in [b"type".as_slice(), b"id", b"status"] {
            if metadata.split(|b| *b == b':').filter(|field| field.split(|b| *b == b'=').next() == Some(key)).count() > 1 {
                return None;
            }
        }
        Some(packet)
    }

    pub fn field(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.metadata.split(|b| *b == b':').find_map(|field| {
            let index = field.iter().position(|b| *b == b'=')?;
            (&field[..index] == key).then_some(&field[index + 1..])
        })
    }

    pub fn id(&self) -> Vec<u8> {
        self.field(b"id").unwrap_or_default().iter().copied().filter(|b| {
            b.is_ascii_alphanumeric() || b"-_+.".contains(b)
        }).collect()
    }

    pub fn with_id(&self, id: Option<&[u8]>, terminator: &[u8]) -> Vec<u8> {
        let mut out = b"\x1b]5522;".to_vec();
        for field in self.metadata.split(|b| *b == b':').filter(|field| !field.starts_with(b"id=")) {
            if out.last() != Some(&b';') { out.push(b':'); }
            out.extend_from_slice(field);
        }
        if let Some(id) = id {
            out.extend_from_slice(b":id=");
            out.extend_from_slice(id);
        }
        if let Some(payload) = self.payload {
            out.push(b';');
            out.extend_from_slice(payload);
        }
        out.extend_from_slice(terminator);
        out
    }
}


/// Per-pane assembly keeps partially written clipboard contents off the host,
/// so other panes cannot splice data into a write. A new write cancels the old.
#[derive(Debug, Clone, Default)]
pub struct WriteBuffer {
    bytes: Vec<u8>,
    started: Option<std::time::Instant>,
    id: Option<Vec<u8>>,
}

impl WriteBuffer {
    pub fn push(&mut self, packet: &Packet<'_>, raw: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
        if packet.field(b"type") == Some(b"write") {
            self.bytes.clear();
            self.started = Some(std::time::Instant::now());
            self.id = packet.field(b"id").map(|_| packet.id());
        } else if self.started.is_none() {
            // As specified, ignore write data until a new write starts.
            return Ok(None);
        } else if self.started.unwrap().elapsed().as_secs() >= 30 {
            self.bytes.clear();
            self.started = None;
            return Err("EBUSY");
        }
        if packet.field(b"type") == Some(b"wdata") && packet.field(b"mime").is_none()
            && !packet.payload.unwrap_or_default().is_empty()
        {
            self.bytes.clear();
            self.started = None;
            return Err("EINVAL");
        }
        if self.bytes.len().saturating_add(raw.len()) > MAX_TRANSFER_BYTES {
            self.bytes.clear();
            self.started = None;
            return Err("EFBIG");
        }
        self.bytes.extend_from_slice(raw);
        if packet.field(b"type") == Some(b"wdata") && packet.field(b"mime").is_none()
            && packet.payload.unwrap_or_default().is_empty()
        {
            self.started = None;
            return Ok(Some(std::mem::take(&mut self.bytes)));
        }
        Ok(None)
    }

    pub fn id(&self) -> Option<&[u8]> {
        self.id.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_replaced_without_reencoding_payload_or_metadata() {
        let packet = Packet::parse(b"5522;type=read:pw=c2VjcmV0:id=app:name=RWRpdG9y;dGV4dC9wbGFpbiBpbWFnZS9wbmc=").unwrap();
        assert_eq!(packet.with_id(Some(b"bridge-1"), b"\x1b\\"), b"\x1b]5522;type=read:pw=c2VjcmV0:name=RWRpdG9y:id=bridge-1;dGV4dC9wbGFpbiBpbWFnZS9wbmc=\x1b\\");
    }

    #[test]
    fn duplicate_ownership_fields_are_rejected() {
        assert!(Packet::parse(b"5522;type=read:id=a:id=b;Lg==").is_none());
        assert!(Packet::parse(b"5522;type=read:type=write;Lg==").is_none());
    }

    #[test]
    fn write_chunks_and_aliases_are_atomic_and_keep_base64_boundaries() {
        let mut buffer = WriteBuffer::default();
        let packets: &[&[u8]] = &[b"5522;type=write", b"5522;type=wdata:mime=aW1hZ2UvcG5n;iVBO", b"5522;type=wdata:mime=aW1hZ2UvcG5n;Rw==", b"5522;type=walias:mime=aW1hZ2UvcG5n;YXBwbGljYXRpb24vb2N0ZXQtc3RyZWFt", b"5522;type=wdata"];
        let mut expected = Vec::new();
        for (index, payload) in packets.iter().enumerate() {
            let raw = [b"\x1b]".as_slice(), payload, b"\x1b\\"].concat();
            expected.extend_from_slice(&raw);
            let result = buffer.push(&Packet::parse(payload).unwrap(), &raw).unwrap();
            if index + 1 == packets.len() { assert_eq!(result, Some(expected.clone())); }
            else { assert_eq!(result, None); }
        }
    }

    #[test]
    fn orphan_data_cannot_join_another_panes_write() {
        let mut buffer = WriteBuffer::default();
        let packet = Packet::parse(b"5522;type=wdata:mime=dGV4dC9wbGFpbg==;YQ==").unwrap();
        assert_eq!(buffer.push(&packet, b"data").unwrap(), None);
    }

    #[test]
    fn restarting_a_write_cancels_incomplete_data_and_timeout_ignores_followups() {
        let mut buffer = WriteBuffer::default();
        let start = Packet::parse(b"5522;type=write:id=first").unwrap();
        buffer.push(&start, b"old start").unwrap();
        buffer.push(&Packet::parse(b"5522;type=wdata:mime=aW1hZ2UvcG5n;YQ==").unwrap(), b"old data").unwrap();
        buffer.push(&Packet::parse(b"5522;type=write:id=second").unwrap(), b"new start").unwrap();
        let end = Packet::parse(b"5522;type=wdata").unwrap();
        assert_eq!(buffer.push(&end, b"end").unwrap(), Some(b"new startend".to_vec()));
        buffer.push(&start, b"start").unwrap();
        buffer.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(31));
        assert_eq!(buffer.push(&end, b"end"), Err("EBUSY"));
        assert_eq!(buffer.id(), Some(b"first".as_slice()));
        assert_eq!(buffer.push(&end, b"end").unwrap(), None);
        assert!(buffer.bytes.is_empty());
    }

    #[test]
    fn write_data_without_a_mime_type_aborts_and_keeps_the_request_id() {
        let mut buffer = WriteBuffer::default();
        let start = Packet::parse(b"5522;type=write:id=owner").unwrap();
        buffer.push(&start, b"\x1b]5522;type=write:id=owner\x1b\\").unwrap();
        let invalid = Packet::parse(b"5522;type=wdata;YQ==").unwrap();
        assert_eq!(buffer.push(&invalid, b"\x1b]5522;type=wdata;YQ==\x1b\\"), Err("EINVAL"));
        assert_eq!(buffer.id(), Some(b"owner".as_slice()));
        assert!(buffer.bytes.is_empty());
        assert_eq!(buffer.push(&invalid, b"ignored").unwrap(), None);
    }
}
