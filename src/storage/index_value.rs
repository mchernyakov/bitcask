//! Hint-file record format (all integers little-endian):
//!
//! [u32 crc][u32 body_len][u32 key_len][key][u64 ts][u64 offset][u64 len]
//!
//! The crc covers `body_len` and the body, not itself.
//!
//! A record does not carry a file id: a hint file is named after the data
//! file it describes (000001.hint -> 000001.data), so the id comes from the
//! file name and is supplied to `deserialize` by the caller.

use super::bytes_util::BytesUtil;
use crate::{KvsError, Result};
use std::io;

const CRC_LEN: usize = 4;
const BODY_LEN_LEN: usize = 4;
const KEY_LEN_LEN: usize = 4;
const INDEX_VALUE_LEN: usize = 24; // ts + offset + len
pub const HEADER_LEN: usize = CRC_LEN + BODY_LEN_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexValue {
    pub file_id: u64,
    pub ts: u64,
    pub offset: u64,
    pub len: usize,
}

impl IndexValue {
    pub fn serialize(&self, key: &[u8]) -> Vec<u8> {
        let body_len = KEY_LEN_LEN + key.len() + INDEX_VALUE_LEN;

        let mut buf = Vec::with_capacity(HEADER_LEN + body_len);
        buf.extend_from_slice(&[0u8; CRC_LEN]);
        buf.extend_from_slice(&(body_len as u32).to_le_bytes());
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(key);
        buf.extend_from_slice(&self.ts.to_le_bytes());
        buf.extend_from_slice(&self.offset.to_le_bytes());
        buf.extend_from_slice(&(self.len as u64).to_le_bytes());

        let crc = crc32fast::hash(&buf[CRC_LEN..]);
        buf[..CRC_LEN].copy_from_slice(&crc.to_le_bytes());

        buf
    }

    pub fn deserialize(buf: &[u8], file_id: u64) -> Result<(&[u8], Self, usize)> {
        let mut cursor = 0;

        let stored_crc = BytesUtil::read_u32(buf, &mut cursor)?;
        let body_len = BytesUtil::read_u32(buf, &mut cursor)? as usize;

        if buf.len() < HEADER_LEN + body_len {
            return Err(KvsError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete index value",
            )));
        }

        let record_end = HEADER_LEN + body_len;
        let actual_crc = crc32fast::hash(&buf[CRC_LEN..record_end]);

        if actual_crc != stored_crc {
            return Err(KvsError::Corruption);
        }

        let key_len = BytesUtil::read_u32(buf, &mut cursor)? as usize;
        let key = BytesUtil::read_bytes(buf, &mut cursor, key_len)?;

        let ts = BytesUtil::read_u64(buf, &mut cursor)?;
        let offset = BytesUtil::read_u64(buf, &mut cursor)?;
        let len = BytesUtil::read_u64(buf, &mut cursor)?;

        let len = usize::try_from(len).map_err(|_| KvsError::Corruption)?;

        if cursor != record_end {
            return Err(KvsError::Corruption);
        }

        Ok((
            key,
            Self {
                file_id,
                ts,
                offset,
                len,
            },
            record_end,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Vec<u8>, IndexValue) {
        let iv = IndexValue {
            file_id: 7,
            ts: 99,
            offset: 4096,
            len: 123,
        };
        (iv.serialize(b"key1"), iv)
    }

    #[track_caller]
    fn assert_corruption(bytes: &[u8]) {
        match IndexValue::deserialize(bytes, 0) {
            Err(KvsError::Corruption) => {}
            other => panic!("expected Corruption, got {other:?}"),
        }
    }

    #[track_caller]
    fn assert_eof(bytes: &[u8]) {
        match IndexValue::deserialize(bytes, 0) {
            Err(KvsError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {}
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[test_log::test]
    fn roundtrip() {
        let (bytes, iv) = sample();
        let (key, decoded, used) = IndexValue::deserialize(&bytes, iv.file_id).unwrap();
        assert_eq!(key, b"key1");
        assert_eq!(decoded, iv);
        assert_eq!(used, bytes.len());
    }

    #[test_log::test]
    fn file_id_comes_from_the_caller_not_the_record() {
        let (bytes, iv) = sample();
        let (_, decoded, _) = IndexValue::deserialize(&bytes, 42).unwrap();
        assert_eq!(decoded.file_id, 42);
        assert_eq!(decoded.ts, iv.ts);
        assert_eq!(decoded.offset, iv.offset);
        assert_eq!(decoded.len, iv.len);
    }

    #[test_log::test]
    fn roundtrip_empty_key_and_extreme_values() {
        let iv = IndexValue {
            file_id: u64::MAX,
            ts: u64::MAX,
            offset: u64::MAX,
            len: usize::MAX,
        };
        let bytes = iv.serialize(b"");
        let (key, decoded, used) = IndexValue::deserialize(&bytes, iv.file_id).unwrap();
        assert_eq!(key, b"");
        assert_eq!(decoded, iv);
        assert_eq!(used, bytes.len());
    }

    #[test_log::test]
    fn deserialize_consumes_exactly_one_record() {
        let first = IndexValue {
            file_id: 1,
            ts: 10,
            offset: 0,
            len: 10,
        };
        let second = IndexValue {
            file_id: 1,
            ts: 20,
            offset: 100,
            len: 20,
        };
        let mut buf = first.serialize(b"alpha");
        let first_len = buf.len();
        buf.extend_from_slice(&second.serialize(b"beta"));

        let (key, decoded, used) = IndexValue::deserialize(&buf, 1).unwrap();
        assert_eq!(key, b"alpha");
        assert_eq!(decoded, first);
        assert_eq!(used, first_len);

        let (key, decoded, used) = IndexValue::deserialize(&buf[first_len..], 1).unwrap();
        assert_eq!(key, b"beta");
        assert_eq!(decoded, second);
        assert_eq!(first_len + used, buf.len());
    }

    #[test_log::test]
    fn every_single_bit_flip_is_rejected() {
        let (bytes, _) = sample();
        for byte_idx in 0..bytes.len() {
            for bit in 0..8 {
                let mut corrupted = bytes.clone();
                corrupted[byte_idx] ^= 1 << bit;
                assert!(
                    IndexValue::deserialize(&corrupted, 0).is_err(),
                    "flip of bit {bit} in byte {byte_idx} was accepted"
                );
            }
        }
    }

    #[test_log::test]
    fn corrupted_crc_is_corruption() {
        let (mut bytes, _) = sample();
        bytes[0] ^= 0xFF;
        assert_corruption(&bytes);
    }

    #[test_log::test]
    fn truncated_buffer_is_eof_not_corruption() {
        let (bytes, _) = sample();
        for prefix_len in 0..bytes.len() {
            assert_eof(&bytes[..prefix_len]);
        }
    }

    #[test_log::test]
    fn valid_crc_but_inconsistent_key_len_is_corruption() {
        let (mut bytes, _) = sample();

        let key_len_pos = HEADER_LEN;
        let key_len = u32::from_le_bytes(bytes[key_len_pos..key_len_pos + 4].try_into().unwrap());
        bytes[key_len_pos..key_len_pos + 4].copy_from_slice(&(key_len - 1).to_le_bytes());

        let crc = crc32fast::hash(&bytes[CRC_LEN..]);
        bytes[..CRC_LEN].copy_from_slice(&crc.to_le_bytes());

        assert_corruption(&bytes);
    }
}
