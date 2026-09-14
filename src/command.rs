//! Log record encoding and decoding.
//!
//! Record format (all integers little-endian):
//!
//! ```text
//! [u32 crc][u32 body_len][body]
//!
//! body (Set): [u64 ts][u8 type=0][u32 key_len][key][u32 value_len][value]
//! body (Rm):  [u64 ts][u8 type=1][u32 key_len][key]
//! ```
//!
//! The crc (CRC-32/IEEE) covers `body_len` and `body`, not itself.
//! `deserialize` returns `Io(UnexpectedEof)` for incomplete records
//! (torn writes) and `Corruption` for records whose bytes don't match
//! their crc — callers rely on that distinction.

use crate::{KvsError, Result};
use std::convert::{TryFrom, TryInto};
use std::io;

/// [u32 crc][u32 body_len] — crc covers body_len and the body.
pub const HEADER_LEN: usize = 8;
const CRC_LEN: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Command<'a> {
    Set {
        ts: u64,
        key: &'a [u8],
        value: &'a [u8],
    },
    Rm {
        ts: u64,
        key: &'a [u8],
    },
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandType {
    Set = 0,
    Rm = 1,
}

impl TryFrom<u8> for CommandType {
    type Error = KvsError;

    fn try_from(value: u8) -> crate::Result<Self> {
        match value {
            0 => Ok(Self::Set),
            1 => Ok(Self::Rm),
            _ => Err(KvsError::UnexpectedCommandType),
        }
    }
}

impl<'a> Command<'a> {
    pub fn get_key(&self) -> &[u8] {
        match self {
            Command::Set { key, .. } => key,
            Command::Rm { key, .. } => key,
        }
    }

    pub fn get_value(&self) -> Option<&[u8]> {
        match self {
            Command::Set { value, .. } => Some(value),
            Command::Rm { .. } => None,
        }
    }

    #[inline]
    fn get_command_type(byte: &u8) -> Result<CommandType> {
        match byte {
            0 => Ok(CommandType::Set),
            1 => Ok(CommandType::Rm),
            _ => Err(KvsError::UnexpectedCommandType),
        }
    }

    pub fn serialize(&self) -> Vec<u8> {
        let body_len = self.encoded_body_len();
        let mut buf = Vec::with_capacity(HEADER_LEN + body_len);
        buf.extend_from_slice(&[0u8; CRC_LEN]); // crc placeholder, backfilled below
        buf.extend_from_slice(&(body_len as u32).to_le_bytes());
        self.write_body(&mut buf);
        let crc = crc32fast::hash(&buf[CRC_LEN..]);
        buf[..CRC_LEN].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    pub fn deserialize(buf: &'a [u8]) -> Result<(Self, usize)> {
        let mut cursor = 0;

        let stored_crc = read_u32(buf, &mut cursor)?;
        let body_len = read_u32(buf, &mut cursor)? as usize;

        if buf.len() < HEADER_LEN + body_len {
            return Err(KvsError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete command",
            )));
        }
        let command_end = HEADER_LEN + body_len;

        let actual_crc = crc32fast::hash(&buf[CRC_LEN..command_end]);
        if actual_crc != stored_crc {
            return Err(KvsError::Corruption);
        }

        let ts = read_u64(buf, &mut cursor)?;
        let command = match CommandType::try_from(read_u8(buf, &mut cursor)?)? {
            CommandType::Set => {
                let key_len = read_u32(buf, &mut cursor)? as usize;
                let key = read_bytes(buf, &mut cursor, key_len)?;
                let value_len = read_u32(buf, &mut cursor)? as usize;
                let value = read_bytes(buf, &mut cursor, value_len)?;

                Command::Set { ts, key, value }
            }
            CommandType::Rm => {
                let key_len = read_u32(buf, &mut cursor)? as usize;
                let key = read_bytes(buf, &mut cursor, key_len)?;
                Command::Rm { ts, key }
            }
        };

        if cursor != command_end {
            return Err(KvsError::Corruption);
        }

        Ok((command, command_end))
    }

    fn encoded_body_len(&self) -> usize {
        match self {
            // 8 bytes for timestamp, 1 byte for command type, 4 bytes for key len, 4 bytes for value len.
            Command::Set { key, value, .. } => 8 + 1 + 4 + key.len() + 4 + value.len(),
            // 8 bytes for timestamp, 1 byte for command type, 4 bytes for key len.
            Command::Rm { key, .. } => 8 + 1 + 4 + key.len(),
        }
    }

    fn write_body(&self, buf: &mut Vec<u8>) {
        match self {
            Command::Set { ts, key, value } => {
                buf.extend_from_slice(&ts.to_le_bytes());
                buf.push(CommandType::Set as u8);
                buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
                buf.extend_from_slice(key);
                buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
                buf.extend_from_slice(value);
            }
            Command::Rm { ts, key } => {
                buf.extend_from_slice(&ts.to_le_bytes());
                buf.push(CommandType::Rm as u8);
                buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
                buf.extend_from_slice(key);
            }
        }
    }
}

#[inline]
fn read_u8(buf: &[u8], cursor: &mut usize) -> io::Result<u8> {
    if *cursor >= buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of buffer",
        ));
    }

    let value = buf[*cursor];
    *cursor += 1;

    Ok(value)
}

#[inline]
fn read_u32(buf: &[u8], cursor: &mut usize) -> io::Result<u32> {
    let end = cursor
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cursor overflow"))?;

    if end > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of buffer",
        ));
    }

    let value = u32::from_le_bytes(buf[*cursor..end].try_into().unwrap());

    *cursor = end;

    Ok(value)
}

#[inline]
fn read_u64(buf: &[u8], cursor: &mut usize) -> io::Result<u64> {
    let end = cursor
        .checked_add(8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "cursor overflow"))?;

    if end > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of buffer",
        ));
    }

    let value = u64::from_le_bytes(buf[*cursor..end].try_into().unwrap());

    *cursor = end;

    Ok(value)
}

#[inline]
fn read_bytes<'a>(buf: &'a [u8], cursor: &mut usize, len: usize) -> io::Result<&'a [u8]> {
    let end = cursor
        .checked_add(len)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "length overflow"))?;

    if end > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "unexpected end of buffer",
        ));
    }

    let value = &buf[*cursor..end];

    *cursor = end;

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KvsError;

    fn sample_set() -> Vec<u8> {
        Command::Set {
            ts: 42,
            key: b"key1",
            value: b"value1",
        }
        .serialize()
    }

    fn raw_record(body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + body.len());
        buf.extend_from_slice(&[0u8; CRC_LEN]);
        buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
        buf.extend_from_slice(body);
        let crc = crc32fast::hash(&buf[CRC_LEN..]);
        buf[..CRC_LEN].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    #[track_caller]
    fn assert_corruption(bytes: &[u8]) {
        match Command::deserialize(bytes) {
            Err(KvsError::Corruption) => {}
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[track_caller]
    fn assert_eof(bytes: &[u8]) {
        match Command::deserialize(bytes) {
            Err(KvsError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {}
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test]
    fn command_serde_roundtrip() {
        let set = Command::Set {
            ts: 42,
            key: b"key1",
            value: b"value1",
        };
        let set_bytes = set.serialize();
        let (set_decoded, set_used) = Command::deserialize(&set_bytes).unwrap();
        assert_eq!(set_decoded, set);
        assert_eq!(set_used, set_bytes.len());

        let rm = Command::Rm {
            ts: 43,
            key: b"key2",
        };
        let rm_bytes = rm.serialize();
        let (rm_decoded, rm_used) = Command::deserialize(&rm_bytes).unwrap();
        assert_eq!(rm_decoded, rm);
        assert_eq!(rm_used, rm_bytes.len());
    }

    #[test]
    fn roundtrip_timestamp_extremes() {
        for ts in [0, 1, u64::MAX] {
            let cmd = Command::Set {
                ts,
                key: b"k",
                value: b"v",
            };
            let bytes = cmd.serialize();
            let (decoded, _) = Command::deserialize(&bytes).unwrap();
            assert_eq!(decoded, cmd);
        }
    }

    #[test]
    fn roundtrip_empty_key_and_value() {
        let set = Command::Set {
            ts: 1,
            key: b"",
            value: b"",
        };
        let bytes = set.serialize();
        let (decoded, used) = Command::deserialize(&bytes).unwrap();
        assert_eq!(decoded, set);
        assert_eq!(used, bytes.len());

        let rm = Command::Rm { ts: 2, key: b"" };
        let bytes = rm.serialize();
        let (decoded, used) = Command::deserialize(&bytes).unwrap();
        assert_eq!(decoded, rm);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn deserialize_consumes_exactly_one_record() {
        let first = Command::Set {
            ts: 1,
            key: b"key1",
            value: b"value1",
        };
        let second = Command::Rm {
            ts: 2,
            key: b"key1",
        };

        let mut buf = first.serialize();
        let first_len = buf.len();
        buf.extend_from_slice(&second.serialize());

        let (decoded, used) = Command::deserialize(&buf).unwrap();
        assert_eq!(decoded, first);
        assert_eq!(used, first_len);

        let (decoded, used) = Command::deserialize(&buf[first_len..]).unwrap();
        assert_eq!(decoded, second);
        assert_eq!(first_len + used, buf.len());
    }

    #[test]
    fn every_single_bit_flip_is_rejected() {
        let bytes = sample_set();
        for byte_idx in 0..bytes.len() {
            for bit in 0..8 {
                let mut corrupted = bytes.clone();
                corrupted[byte_idx] ^= 1 << bit;
                assert!(
                    Command::deserialize(&corrupted).is_err(),
                    "flip of bit {bit} in byte {byte_idx} was accepted"
                );
            }
        }
    }

    #[test]
    fn corrupted_crc_field_is_corruption() {
        let mut bytes = sample_set();
        bytes[0] ^= 0xFF;
        assert_corruption(&bytes);
    }

    #[test]
    fn corrupted_body_is_corruption() {
        let mut bytes = sample_set();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert_corruption(&bytes);

        let mut bytes = sample_set();
        bytes[HEADER_LEN] ^= 0x01;
        assert_corruption(&bytes);
    }

    #[test]
    fn truncated_buffer_is_eof_not_corruption() {
        let bytes = sample_set();
        for prefix_len in 0..bytes.len() {
            assert_eof(&bytes[..prefix_len]);
        }
    }

    #[test]
    fn oversized_body_len_is_eof() {
        let mut bytes = sample_set();
        bytes[CRC_LEN..HEADER_LEN].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eof(&bytes);
    }

    #[test]
    fn unknown_command_type_with_valid_crc_is_rejected() {
        let mut body = Vec::new();
        body.extend_from_slice(&42u64.to_le_bytes());
        body.push(7);
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(b"k");

        let bytes = raw_record(&body);
        match Command::deserialize(&bytes) {
            Err(KvsError::UnexpectedCommandType) => {}
            other => panic!("expected UnexpectedCommandType, got {other:?}"),
        }
    }

    #[test]
    fn valid_crc_but_inconsistent_inner_lengths_is_corruption() {
        let mut body = Vec::new();
        body.extend_from_slice(&42u64.to_le_bytes());
        body.push(CommandType::Rm as u8);
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(b"ab");

        assert_corruption(&raw_record(&body));
    }
}
