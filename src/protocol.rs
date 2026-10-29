//! Wire format of the Galdr protocol, version 1 (see docs/PROTOCOL.md).
//!
//! Discovery (UDP): `GLDR | 0x10 | version` → `GLDR | 0x11 | version | control port | name`.
//! Control (TCP):   `type:u8 | len:u16 | payload[len]`.
//! Media (UDP):     `GLDR | 0x20 | flags | seq:u32 | timestamp:u32 | opus[..]`.

use std::io::{self, Read, Write};

pub const MAGIC: &[u8; 4] = b"GLDR";
pub const VERSION: u8 = 1;

pub const DEFAULT_CONTROL_PORT: u16 = 47800;
/// The phone listens for discovery requests on the same number as its control port (UDP).
pub const DISCOVERY_PORT: u16 = 47800;
pub const DEFAULT_MEDIA_PORT: u16 = 47801;

pub const MSG_HELLO: u8 = 0x01;
pub const MSG_WELCOME: u8 = 0x02;
pub const MSG_REJECT: u8 = 0x03;
pub const MSG_PING: u8 = 0x04;

const DISCOVERY_REQUEST: u8 = 0x10;
const DISCOVERY_REPLY: u8 = 0x11;
const MEDIA: u8 = 0x20;
const MEDIA_HEADER_LEN: usize = 14;

pub const CODEC_OPUS: u8 = 1;

/// Largest control payload we accept; real ones are a few dozen bytes.
const MAX_CONTROL_PAYLOAD: usize = 4096;

pub fn encode_message(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + payload.len());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn write_message<W: Write>(w: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&encode_message(kind, payload))
}

pub fn read_message<R: Read>(r: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 3];
    r.read_exact(&mut header)?;
    let len = u16::from_be_bytes([header[1], header[2]]) as usize;
    if len > MAX_CONTROL_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("control message 0x{:02x} too long: {len} bytes", header[0]),
        ));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// Strips `GLDR` + version from the start of a payload.
fn after_magic(payload: &[u8]) -> Option<(u8, &[u8])> {
    let rest = payload.strip_prefix(MAGIC.as_slice())?;
    let (&version, rest) = rest.split_first()?;
    Some((version, rest))
}

pub fn hello_payload(media_port: u16, pc_name: &str) -> Vec<u8> {
    let mut p = MAGIC.to_vec();
    p.push(VERSION);
    p.extend_from_slice(&media_port.to_be_bytes());
    p.extend_from_slice(pc_name.as_bytes());
    p
}

/// Audio format and name the phone announces in `WELCOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    pub version: u8,
    pub codec: u8,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_ms: u8,
    pub phone_name: String,
}

pub fn parse_welcome(payload: &[u8]) -> Option<Welcome> {
    let (version, rest) = after_magic(payload)?;
    if rest.len() < 7 {
        return None;
    }
    Some(Welcome {
        version,
        codec: rest[0],
        sample_rate: u32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]),
        channels: rest[5],
        frame_ms: rest[6],
        phone_name: String::from_utf8_lossy(&rest[7..]).into_owned(),
    })
}

pub fn discovery_request() -> [u8; 6] {
    let mut d = [0u8; 6];
    d[..4].copy_from_slice(MAGIC);
    d[4] = DISCOVERY_REQUEST;
    d[5] = VERSION;
    d
}

/// A phone that answered a discovery request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryReply {
    pub control_port: u16,
    pub name: String,
}

pub fn parse_discovery_reply(datagram: &[u8]) -> Option<DiscoveryReply> {
    let rest = datagram.strip_prefix(MAGIC.as_slice())?;
    let (&kind, rest) = rest.split_first()?;
    let (&version, rest) = rest.split_first()?;
    if kind != DISCOVERY_REPLY || version != VERSION || rest.len() < 2 {
        return None;
    }
    Some(DiscoveryReply {
        control_port: u16::from_be_bytes([rest[0], rest[1]]),
        name: String::from_utf8_lossy(&rest[2..]).into_owned(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaPacket<'a> {
    pub seq: u32,
    /// Of the frame's first sample, in samples.
    pub timestamp: u32,
    pub opus: &'a [u8],
}

pub fn parse_media(datagram: &[u8]) -> Option<MediaPacket<'_>> {
    if datagram.len() < MEDIA_HEADER_LEN || &datagram[..4] != MAGIC || datagram[4] != MEDIA {
        return None;
    }
    let u32_at = |i: usize| u32::from_be_bytes(datagram[i..i + 4].try_into().unwrap());
    Some(MediaPacket {
        seq: u32_at(6),
        timestamp: u32_at(10),
        opus: &datagram[MEDIA_HEADER_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn hello_matches_spec_example() {
        assert_eq!(
            encode_message(MSG_HELLO, &hello_payload(47801, "DESKTOP")),
            hex("01 000e 474c4452 01 bab9 4445534b544f50")
        );
        assert_eq!(encode_message(MSG_PING, &[]), hex("04 0000"));
    }

    #[test]
    fn reads_welcome_from_spec_example() {
        let mut stream = io::Cursor::new(hex(
            "02 0013 474c4452 01 01 0000bb80 01 14 506978656c2037 04 0000",
        ));
        let (kind, payload) = read_message(&mut stream).unwrap();
        assert_eq!(kind, MSG_WELCOME);
        assert_eq!(
            parse_welcome(&payload).unwrap(),
            Welcome {
                version: 1,
                codec: CODEC_OPUS,
                sample_rate: 48000,
                channels: 1,
                frame_ms: 20,
                phone_name: "Pixel 7".into(),
            }
        );
        assert_eq!(read_message(&mut stream).unwrap(), (MSG_PING, vec![]));
        assert!(read_message(&mut stream).is_err());
    }

    #[test]
    fn rejects_bad_welcome() {
        assert!(parse_welcome(&hex("474c4451 01 01 0000bb80 01 14")).is_none());
        assert!(parse_welcome(&hex("474c4452 01 01 0000bb")).is_none());
    }

    #[test]
    fn discovery_round_trip() {
        assert_eq!(discovery_request().to_vec(), hex("474c4452 10 01"));
        assert_eq!(
            parse_discovery_reply(&hex("474c4452 11 01 bab8 506978656c2037")).unwrap(),
            DiscoveryReply {
                control_port: 47800,
                name: "Pixel 7".into(),
            }
        );
        assert!(parse_discovery_reply(&hex("474c4452 10 01")).is_none());
        assert!(parse_discovery_reply(&hex("474c4452 11 02 bab8")).is_none());
    }

    #[test]
    fn parses_media_packet() {
        let d = hex("474c4452 20 00 00000102 0000f000 f8fffe");
        let p = parse_media(&d).unwrap();
        assert_eq!(p.seq, 0x102);
        assert_eq!(p.timestamp, 0xf000);
        assert_eq!(p.opus, &[0xf8, 0xff, 0xfe]);
    }

    #[test]
    fn rejects_bad_media() {
        assert!(parse_media(&hex("474c4452 21 00 00000102 0000f000 f8")).is_none());
        assert!(parse_media(&hex("474c4451 20 00 00000102 0000f000 f8")).is_none());
        assert!(parse_media(&hex("474c4452 20 00 0000")).is_none());
    }
}
