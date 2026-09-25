//! Frame: [u8 msg_type][u32 LE payload_len][payload]
//!
//! Requests  (msg_type): 0 Get  [key]
//!                       1 Set  [u32 key_len][key][value]
//!                       2 Rm   [key]
//! Responses (msg_type): 0 Ok
//!                       1 Value [value]
//!                       2 Error [u8 code]

use super::error::ErrorCode;
use crate::{KvStore, KvsError, Result};
use std::io::{self, Read, Write};

pub const MAX_PAYLOAD: u32 = 16 << 20;

const REQ_GET: u8 = 0;
const REQ_SET: u8 = 1;
const REQ_RM: u8 = 2;

const RESP_OK: u8 = 0;
const RESP_VALUE: u8 = 1;
const RESP_ERROR: u8 = 2;

#[derive(Debug, PartialEq, Eq)]
pub enum Request<'a> {
    Get { key: &'a str },
    Set { key: &'a str, value: &'a str },
    Rm { key: &'a str },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Response {
    Ok,
    Value(String),
    Error(ErrorCode),
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    // EOF before the first byte is a clean close. EOF anywhere after that
    // is a torn frame and must surface as an error.
    let mut msg_type = [0u8; 1];
    match reader.read_exact(&mut msg_type) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let msg_type = msg_type[0];

    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_PAYLOAD {
        return Err(KvsError::InvalidData("payload too large".into()));
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload)?;
    Ok(Some((msg_type, payload)))
}

pub fn write_frame<W: Write>(writer: &mut W, msg_type: u8, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| KvsError::InvalidData("payload too large".into()))?;
    writer.write_all(&[msg_type])?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn write_request<W: Write>(writer: &mut W, req: &Request) -> Result<()> {
    match req {
        Request::Get { key } => write_frame(writer, REQ_GET, key.as_bytes()),
        Request::Rm { key } => write_frame(writer, REQ_RM, key.as_bytes()),
        Request::Set { key, value } => {
            let mut buf = Vec::with_capacity(4 + key.len() + value.len());
            buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
            buf.extend_from_slice(value.as_bytes());
            write_frame(writer, REQ_SET, &buf)
        }
    }
}

pub fn decode_request(msg_type: u8, payload: &[u8]) -> Result<Request<'_>> {
    match msg_type {
        REQ_GET => Ok(Request::Get {
            key: utf8(payload)?,
        }),
        REQ_RM => Ok(Request::Rm {
            key: utf8(payload)?,
        }),
        REQ_SET => {
            let len_bytes: [u8; 4] = payload
                .get(..4)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(|| KvsError::InvalidData("short set payload".into()))?;
            let key_len = u32::from_le_bytes(len_bytes) as usize;
            let rest = &payload[4..];
            if rest.len() < key_len {
                return Err(KvsError::InvalidData("short set payload".into()));
            }
            let (key, value) = rest.split_at(key_len);
            Ok(Request::Set {
                key: utf8(key)?,
                value: utf8(value)?,
            })
        }
        _ => Err(KvsError::UnexpectedCommandType),
    }
}

pub fn write_response<W: Write>(writer: &mut W, resp: &Response) -> Result<()> {
    match resp {
        Response::Ok => write_frame(writer, RESP_OK, &[]),
        Response::Value(v) => write_frame(writer, RESP_VALUE, v.as_bytes()),
        Response::Error(code) => write_frame(writer, RESP_ERROR, &[*code as u8]),
    }
}

pub fn decode_response(msg_type: u8, payload: &[u8]) -> Result<Response> {
    match msg_type {
        RESP_OK => Ok(Response::Ok),
        RESP_VALUE => Ok(Response::Value(String::from_utf8(payload.to_vec())?)),
        RESP_ERROR => {
            let code = *payload
                .first()
                .ok_or_else(|| KvsError::InvalidData("missing error code".into()))?;
            Ok(Response::Error(ErrorCode::try_from(code)?))
        }
        _ => Err(KvsError::UnexpectedCommandType),
    }
}

pub fn execute<S: KvStore>(store: &S, req: Request) -> Response {
    let result = match req {
        Request::Get { key } => store.get(key).map(|v| match v {
            Some(v) => Response::Value(v),
            None => Response::Error(ErrorCode::KeyNotFound),
        }),
        Request::Set { key, value } => store.set(key, value).map(|()| Response::Ok),
        Request::Rm { key } => store.remove(key).map(|()| Response::Ok),
    };
    result.unwrap_or_else(|e| Response::Error(ErrorCode::from(&e)))
}

fn utf8(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).map_err(|_| KvsError::InvalidData("invalid utf-8".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const HEADER_LEN: usize = 5;

    fn frame(msg_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg_type, payload).unwrap();
        buf
    }

    fn request_roundtrip(req: &Request) {
        let mut wire = Vec::new();
        write_request(&mut wire, req).unwrap();

        let mut cursor = Cursor::new(&wire);
        let (msg_type, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(
            cursor.position() as usize,
            wire.len(),
            "frame not fully consumed"
        );
        assert_eq!(&decode_request(msg_type, &payload).unwrap(), req);
    }

    fn response_roundtrip(resp: &Response) {
        let mut wire = Vec::new();
        write_response(&mut wire, resp).unwrap();

        let mut cursor = Cursor::new(&wire);
        let (msg_type, payload) = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(
            cursor.position() as usize,
            wire.len(),
            "frame not fully consumed"
        );
        assert_eq!(&decode_response(msg_type, &payload).unwrap(), resp);
    }

    #[track_caller]
    fn assert_invalid_data<T: std::fmt::Debug>(r: Result<T>) {
        match r {
            Err(KvsError::InvalidData(_)) => {}
            other => panic!("expected InvalidData, got {other:?}"),
        }
    }

    #[track_caller]
    fn assert_eof<T: std::fmt::Debug>(r: Result<T>) {
        match r {
            Err(KvsError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {}
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    // ---------- framing ----------

    #[test_log::test]
    fn frame_layout_is_type_then_le_len_then_payload() {
        let wire = frame(7, b"abc");
        assert_eq!(wire, [7, 3, 0, 0, 0, b'a', b'b', b'c']);
    }

    #[test_log::test]
    fn empty_stream_is_clean_eof() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test_log::test]
    fn empty_payload_frame_roundtrips() {
        let wire = frame(RESP_OK, &[]);
        assert_eq!(wire.len(), HEADER_LEN);
        let (ty, payload) = read_frame(&mut Cursor::new(wire)).unwrap().unwrap();
        assert_eq!(ty, RESP_OK);
        assert!(payload.is_empty());
    }

    #[test_log::test]
    fn truncated_frame_is_eof_not_none() {
        let wire = frame(REQ_GET, b"key");
        // Every strict prefix except the empty one is a torn frame.
        for prefix_len in 1..wire.len() {
            assert_eof(read_frame(&mut Cursor::new(&wire[..prefix_len])));
        }
    }

    #[test_log::test]
    fn multiple_frames_are_read_in_order() {
        let mut wire = frame(1, b"first");
        wire.extend_from_slice(&frame(2, b""));
        wire.extend_from_slice(&frame(3, b"third"));

        let mut cursor = Cursor::new(wire);
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((1, b"first".to_vec()))
        );
        assert_eq!(read_frame(&mut cursor).unwrap(), Some((2, Vec::new())));
        assert_eq!(
            read_frame(&mut cursor).unwrap(),
            Some((3, b"third".to_vec()))
        );
        assert_eq!(read_frame(&mut cursor).unwrap(), None);
    }

    #[test_log::test]
    fn oversized_length_is_rejected_before_allocating() {
        let mut header = vec![REQ_GET];
        header.extend_from_slice(&(MAX_PAYLOAD + 1).to_le_bytes());
        assert_invalid_data(read_frame(&mut Cursor::new(header)));

        let mut header = vec![REQ_GET];
        header.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_invalid_data(read_frame(&mut Cursor::new(header)));
    }

    // ---------- requests ----------

    #[test_log::test]
    fn request_roundtrip_all_variants() {
        request_roundtrip(&Request::Get { key: "key1" });
        request_roundtrip(&Request::Rm { key: "key2" });
        request_roundtrip(&Request::Set {
            key: "key3",
            value: "value3",
        });
    }

    #[test_log::test]
    fn request_roundtrip_empty_and_unicode() {
        request_roundtrip(&Request::Get { key: "" });
        request_roundtrip(&Request::Set { key: "", value: "" });
        request_roundtrip(&Request::Set {
            key: "ключ",
            value: "значение 🚀",
        });
    }

    #[test_log::test]
    fn set_payload_layout() {
        let mut wire = Vec::new();
        write_request(
            &mut wire,
            &Request::Set {
                key: "ab",
                value: "xyz",
            },
        )
        .unwrap();
        let payload = &wire[HEADER_LEN..];
        assert_eq!(payload, [2, 0, 0, 0, b'a', b'b', b'x', b'y', b'z']);
    }

    #[test_log::test]
    fn set_with_value_containing_key_bytes_is_unambiguous() {
        // Value starts with the same bytes as the key; length prefix must win.
        request_roundtrip(&Request::Set {
            key: "aa",
            value: "aaaa",
        });
    }

    #[test_log::test]
    fn unknown_request_type_is_rejected() {
        match decode_request(200, b"key") {
            Err(KvsError::UnexpectedCommandType) => {}
            other => panic!("expected UnexpectedCommandType, got {other:?}"),
        }
    }

    #[test_log::test]
    fn invalid_utf8_key_is_rejected() {
        assert_invalid_data(decode_request(REQ_GET, &[0xff, 0xfe]));
        assert_invalid_data(decode_request(REQ_RM, &[0xff, 0xfe]));

        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.extend_from_slice(&[0xff]); // key
        payload.extend_from_slice(b"ok"); // value
        assert_invalid_data(decode_request(REQ_SET, &payload));

        let mut payload = 1u32.to_le_bytes().to_vec();
        payload.extend_from_slice(b"k");
        payload.extend_from_slice(&[0xff]); // value
        assert_invalid_data(decode_request(REQ_SET, &payload));
    }

    #[test_log::test]
    fn set_payload_shorter_than_length_prefix_is_rejected() {
        for len in 0..4 {
            assert_invalid_data(decode_request(REQ_SET, &vec![0u8; len]));
        }
    }

    #[test_log::test]
    fn set_key_len_exceeding_payload_is_rejected() {
        let mut payload = 10u32.to_le_bytes().to_vec();
        payload.extend_from_slice(b"short");
        assert_invalid_data(decode_request(REQ_SET, &payload));

        let mut payload = u32::MAX.to_le_bytes().to_vec();
        payload.extend_from_slice(b"k");
        assert_invalid_data(decode_request(REQ_SET, &payload));
    }

    // ---------- responses ----------

    #[test_log::test]
    fn response_roundtrip_all_variants() {
        response_roundtrip(&Response::Ok);
        response_roundtrip(&Response::Value("value".into()));
        response_roundtrip(&Response::Value(String::new()));
        for code in [
            ErrorCode::KeyNotFound,
            ErrorCode::Internal,
            ErrorCode::InvalidRequest,
        ] {
            response_roundtrip(&Response::Error(code));
        }
    }

    #[test_log::test]
    fn error_response_is_single_code_byte() {
        let mut wire = Vec::new();
        write_response(&mut wire, &Response::Error(ErrorCode::KeyNotFound)).unwrap();
        assert_eq!(&wire[HEADER_LEN..], &[ErrorCode::KeyNotFound as u8]);
    }

    #[test_log::test]
    fn unknown_response_type_is_rejected() {
        match decode_response(200, b"") {
            Err(KvsError::UnexpectedCommandType) => {}
            other => panic!("expected UnexpectedCommandType, got {other:?}"),
        }
    }

    #[test_log::test]
    fn error_response_without_code_is_rejected() {
        assert_invalid_data(decode_response(RESP_ERROR, &[]));
    }

    #[test_log::test]
    fn error_response_with_unknown_code_is_rejected() {
        assert!(decode_response(RESP_ERROR, &[0]).is_err());
        assert!(decode_response(RESP_ERROR, &[42]).is_err());
    }

    #[test_log::test]
    fn value_response_must_be_utf8() {
        match decode_response(RESP_VALUE, &[0xff]) {
            Err(KvsError::Utf8(_)) => {}
            other => panic!("expected Utf8, got {other:?}"),
        }
    }

    // ---------- dispatch ----------

    #[derive(Clone)]
    struct FakeStore {
        map: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
        fail_with: Option<fn() -> KvsError>,
    }

    impl FakeStore {
        fn new() -> Self {
            Self {
                map: Default::default(),
                fail_with: None,
            }
        }
    }

    impl KvStore for FakeStore {
        fn open(_: crate::Config) -> Result<Self> {
            Ok(Self::new())
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            if let Some(f) = self.fail_with {
                return Err(f());
            }
            self.map.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn remove(&self, key: &str) -> Result<()> {
            if let Some(f) = self.fail_with {
                return Err(f());
            }
            self.map
                .lock()
                .unwrap()
                .remove(key)
                .map(|_| ())
                .ok_or(KvsError::KeyNotFound)
        }
        fn get(&self, key: &str) -> Result<Option<String>> {
            if let Some(f) = self.fail_with {
                return Err(f());
            }
            Ok(self.map.lock().unwrap().get(key).cloned())
        }
    }

    #[test_log::test]
    fn execute_maps_store_results_to_responses() {
        let store = FakeStore::new();

        assert_eq!(
            execute(&store, Request::Get { key: "k" }),
            Response::Error(ErrorCode::KeyNotFound)
        );
        assert_eq!(
            execute(&store, Request::Rm { key: "k" }),
            Response::Error(ErrorCode::KeyNotFound)
        );
        assert_eq!(
            execute(
                &store,
                Request::Set {
                    key: "k",
                    value: "v"
                }
            ),
            Response::Ok
        );
        assert_eq!(
            execute(&store, Request::Get { key: "k" }),
            Response::Value("v".into())
        );
        assert_eq!(execute(&store, Request::Rm { key: "k" }), Response::Ok);
        assert_eq!(
            execute(&store, Request::Get { key: "k" }),
            Response::Error(ErrorCode::KeyNotFound)
        );
    }

    #[test_log::test]
    fn execute_maps_internal_store_errors_to_internal_code() {
        let mut store = FakeStore::new();
        store.fail_with = Some(|| KvsError::Corruption);
        assert_eq!(
            execute(&store, Request::Get { key: "k" }),
            Response::Error(ErrorCode::Internal)
        );

        store.fail_with = Some(|| KvsError::Io(io::Error::other("disk on fire")));
        assert_eq!(
            execute(
                &store,
                Request::Set {
                    key: "k",
                    value: "v"
                }
            ),
            Response::Error(ErrorCode::Internal)
        );
    }

    #[test_log::test]
    fn full_request_response_cycle_over_a_byte_stream() {
        let store = FakeStore::new();

        // Client side: three requests back to back on one connection.
        let mut inbound = Vec::new();
        write_request(
            &mut inbound,
            &Request::Set {
                key: "a",
                value: "1",
            },
        )
        .unwrap();
        write_request(&mut inbound, &Request::Get { key: "a" }).unwrap();
        write_request(&mut inbound, &Request::Get { key: "missing" }).unwrap();

        // Server side: the same loop as kvs-server::handle.
        let mut reader = Cursor::new(inbound);
        let mut outbound = Vec::new();
        while let Some((ty, payload)) = read_frame(&mut reader).unwrap() {
            let resp = match decode_request(ty, &payload) {
                Ok(req) => execute(&store, req),
                Err(_) => Response::Error(ErrorCode::InvalidRequest),
            };
            write_response(&mut outbound, &resp).unwrap();
        }

        // Client side: decode the replies in order.
        let mut reader = Cursor::new(outbound);
        let mut replies = Vec::new();
        while let Some((ty, payload)) = read_frame(&mut reader).unwrap() {
            replies.push(decode_response(ty, &payload).unwrap());
        }
        assert_eq!(
            replies,
            [
                Response::Ok,
                Response::Value("1".into()),
                Response::Error(ErrorCode::KeyNotFound),
            ]
        );
    }
}
