//! RESP2 subset for `redis-cli` / `redis-benchmark`: GET, SET, DEL.
//!
//! Types by first byte, `\r\n`-terminated:
//!
//! ```text
//! +OK\r\n                          simple string
//! -ERR message\r\n                 error
//! :42\r\n                          integer
//! $5\r\nhello\r\n                  bulk string (byte length, data, \r\n)
//! $-1\r\n                          null bulk string ("nil")
//! *2\r\n$3\r\nGET\r\n$1\r\nk\r\n   array
//! ```
//!
//! A request is an array of bulk strings: command name, then arguments.
//! Flow: `read_frame_resp -> decode_request_resp -> execute -> write_response_resp`.
//! A malformed frame is a protocol error (server closes the connection);
//! a bad command is `InvalidData` (server replies `-ERR` and continues).

use super::error::ErrorCode;
use super::protocol::{Request, Response, MAX_PAYLOAD};
use crate::network::protocol;
use crate::{KvsError, Result};
use std::io::{self, BufRead, Write};

pub const MAX_ARRAY_LEN: usize = 1 << 20; // 1M
const MAX_DEPTH: usize = 32;

#[derive(Debug, PartialEq)]
pub enum RespValue<'a> {
    SimpleString(&'a [u8]),
    Error(&'a [u8]),
    Integer(i64),
    BulkString(Option<&'a [u8]>),
    Array(Option<Vec<RespValue<'a>>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespCommand {
    Get,
    Set,
    Del,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RespRequest<'a> {
    Kv(Request<'a>),
    /// `CONFIG GET name...`; redis-benchmark sends it on startup.
    ConfigGet(Vec<&'a [u8]>),
}

impl From<&Request<'_>> for RespCommand {
    fn from(req: &Request<'_>) -> Self {
        match req {
            Request::Get { .. } => RespCommand::Get,
            Request::Set { .. } => RespCommand::Set,
            Request::Rm { .. } => RespCommand::Del,
        }
    }
}

pub fn is_resp(msg_type: u8) -> bool {
    match msg_type {
        b'+' | b'-' | b':' | b'$' | b'*' => true,
        _ => false,
    }
}

pub fn read_frame_resp<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    let mut pending = 1usize;

    while pending > 0 {
        pending -= 1;

        let line_start = frame.len();
        let n = reader.read_until(b'\n', &mut frame)?;
        if n == 0 {
            if line_start == 0 {
                return Ok(None);
            }
            return Err(unexpected_eof());
        }
        let line = &frame[line_start..];
        if line.len() < 3 || !line.ends_with(b"\r\n") {
            return Err(protocol_error("expected CRLF-terminated line"));
        }
        let (kind, body) = (line[0], &line[1..line.len() - 2]);

        match kind {
            b'+' | b'-' | b':' => {}
            b'$' => {
                if let Some(len) = parse_len(body)? {
                    if len > MAX_PAYLOAD as usize {
                        return Err(protocol_error("bulk string too large"));
                    }
                    let data_start = frame.len();
                    frame.resize(data_start + len + 2, 0);
                    reader.read_exact(&mut frame[data_start..])?;
                    if !frame.ends_with(b"\r\n") {
                        return Err(protocol_error("bulk string not CRLF-terminated"));
                    }
                }
            }
            b'*' => {
                if let Some(len) = parse_len(body)? {
                    if len > MAX_ARRAY_LEN || pending + len > MAX_ARRAY_LEN {
                        return Err(protocol_error("array too large"));
                    }
                    pending += len;
                }
            }
            _ => return Err(protocol_error("unknown type byte")),
        }
    }
    Ok(Some(frame))
}

/// `$` / `*` length; `-1` is null.
fn parse_len(body: &[u8]) -> Result<Option<usize>> {
    if body == b"-1" {
        return Ok(None);
    }
    if body.is_empty() || body.len() > 10 || !body.iter().all(u8::is_ascii_digit) {
        return Err(protocol_error("invalid length"));
    }
    let mut len = 0usize;
    for &d in body {
        len = len * 10 + (d - b'0') as usize;
    }
    Ok(Some(len))
}

pub fn parse_resp(buf: &[u8]) -> Result<RespValue<'_>> {
    let (value, used) = parse_value(buf, 0)?;
    if used != buf.len() {
        return Err(protocol_error("trailing bytes after value"));
    }
    Ok(value)
}

fn parse_value(buf: &[u8], depth: usize) -> Result<(RespValue<'_>, usize)> {
    if depth > MAX_DEPTH {
        return Err(protocol_error("nesting too deep"));
    }
    let (kind, body, mut used) = take_line(buf)?;
    let value = match kind {
        b'+' => RespValue::SimpleString(body),
        b'-' => RespValue::Error(body),
        b':' => RespValue::Integer(parse_i64(body)?),
        b'$' => match parse_len(body)? {
            None => RespValue::BulkString(None),
            Some(len) => {
                let data = buf
                    .get(used..used + len)
                    .ok_or_else(|| protocol_error("short bulk string"))?;
                if buf.get(used + len..used + len + 2) != Some(b"\r\n") {
                    return Err(protocol_error("bulk string not CRLF-terminated"));
                }
                used += len + 2;
                RespValue::BulkString(Some(data))
            }
        },
        b'*' => match parse_len(body)? {
            None => RespValue::Array(None),
            Some(len) => {
                let mut items = Vec::with_capacity(len.min(64));
                for _ in 0..len {
                    let (item, n) = parse_value(&buf[used..], depth + 1)?;
                    items.push(item);
                    used += n;
                }
                RespValue::Array(Some(items))
            }
        },
        _ => return Err(protocol_error("unknown type byte")),
    };
    Ok((value, used))
}

/// `<kind><body>\r\n` -> (kind, body, bytes consumed).
fn take_line(buf: &[u8]) -> Result<(u8, &[u8], usize)> {
    let end = buf
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| protocol_error("expected CRLF-terminated line"))?;
    if end == 0 {
        return Err(protocol_error("empty line"));
    }
    Ok((buf[0], &buf[1..end], end + 2))
}

#[inline(always)]
fn parse_i64(body: &[u8]) -> Result<i64> {
    std::str::from_utf8(body)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| protocol_error("invalid integer"))
}

pub fn decode_request_resp(frame: &[u8]) -> Result<RespRequest<'_>> {
    let value = parse_resp(frame)?;
    let items = match value {
        RespValue::Array(Some(items)) if !items.is_empty() => items,
        _ => return Err(protocol_error("expected non-empty array")),
    };
    let args = items
        .iter()
        .map(|v| match v {
            RespValue::BulkString(Some(b)) => Ok(*b),
            _ => Err(protocol_error("expected bulk string")),
        })
        .collect::<Result<Vec<&[u8]>>>()?;
    let (name, args) = (args[0], &args[1..]);

    let check = |expected: usize| -> Result<()> {
        if args.len() == expected {
            Ok(())
        } else {
            Err(KvsError::InvalidData(format!(
                "wrong number of arguments for '{}' command",
                String::from_utf8_lossy(name).to_ascii_lowercase()
            )))
        }
    };

    if name.eq_ignore_ascii_case(b"GET") {
        check(1)?;
        Ok(RespRequest::Kv(Request::Get {
            key: protocol::utf8(args[0])?,
        }))
    } else if name.eq_ignore_ascii_case(b"SET") {
        check(2)?;
        Ok(RespRequest::Kv(Request::Set {
            key: protocol::utf8(args[0])?,
            value: protocol::utf8(args[1])?,
        }))
    } else if name.eq_ignore_ascii_case(b"DEL") {
        check(1)?;
        Ok(RespRequest::Kv(Request::Rm {
            key: protocol::utf8(args[0])?,
        }))
    } else if name.eq_ignore_ascii_case(b"CONFIG") {
        match args.first() {
            Some(sub) if sub.eq_ignore_ascii_case(b"GET") => {
                Ok(RespRequest::ConfigGet(args[1..].to_vec()))
            }
            _ => Err(KvsError::InvalidData(
                "unknown subcommand for 'config' command".into(),
            )),
        }
    } else {
        Err(KvsError::InvalidData(format!(
            "unknown command '{}'",
            String::from_utf8_lossy(name)
        )))
    }
}

/// ```text
///                     GET          SET     DEL
/// Ok                  -            +OK     :1
/// Value(v)            $len\r\nv    -       -
/// Error(KeyNotFound)  $-1          -       :0
/// Error(other)        -ERR ...     -ERR    -ERR
/// ```
pub fn write_response_resp<W: Write>(
    writer: &mut W,
    cmd: RespCommand,
    resp: &Response,
) -> Result<()> {
    match (cmd, resp) {
        (_, Response::Value(v)) => {
            write!(writer, "${}\r\n", v.len())?;
            writer.write_all(v.as_bytes())?;
            writer.write_all(b"\r\n")?;
        }
        (RespCommand::Del, Response::Ok) => writer.write_all(b":1\r\n")?,
        (RespCommand::Del, Response::Error(ErrorCode::KeyNotFound)) => {
            writer.write_all(b":0\r\n")?
        }
        (RespCommand::Get, Response::Error(ErrorCode::KeyNotFound)) => {
            writer.write_all(b"$-1\r\n")?
        }
        (_, Response::Ok) => writer.write_all(b"+OK\r\n")?,
        (_, Response::Error(code)) => write_error(writer, &code.to_string())?,
    }
    Ok(())
}

/// `[name, "", name, "", ...]`: redis-benchmark needs a name/value pair per name.
pub fn write_config_get<W: Write>(writer: &mut W, names: &[&[u8]]) -> Result<()> {
    write!(writer, "*{}\r\n", names.len() * 2)?;
    for name in names {
        write!(writer, "${}\r\n", name.len())?;
        writer.write_all(name)?;
        writer.write_all(b"\r\n$0\r\n\r\n")?;
    }
    Ok(())
}

pub fn write_error<W: Write>(writer: &mut W, msg: &str) -> Result<()> {
    let msg = msg.replace(['\r', '\n'], " ");
    write!(writer, "-ERR {msg}\r\n")?;
    Ok(())
}

pub fn decode_error_message(err: &KvsError) -> String {
    match err {
        KvsError::InvalidData(msg) => msg.clone(),
        other => other.to_string(),
    }
}

fn protocol_error(msg: &str) -> KvsError {
    KvsError::InvalidData(format!("Protocol error: {msg}"))
}

fn unexpected_eof() -> KvsError {
    io::Error::new(io::ErrorKind::UnexpectedEof, "torn RESP frame").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::execute;
    use crate::KvStore;
    use std::io::Cursor;

    fn frame(bytes: &[u8]) -> Vec<u8> {
        read_frame_resp(&mut Cursor::new(bytes)).unwrap().unwrap()
    }

    fn encoded(cmd: RespCommand, resp: &Response) -> Vec<u8> {
        let mut out = Vec::new();
        write_response_resp(&mut out, cmd, resp).unwrap();
        out
    }

    #[track_caller]
    fn assert_protocol_error<T: std::fmt::Debug>(r: Result<T>) {
        match r {
            Err(KvsError::InvalidData(m)) if m.starts_with("Protocol error") => {}
            other => panic!("expected protocol error, got {other:?}"),
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
    fn frame_reads_exactly_one_value() {
        let wire = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n+OK\r\n";
        let mut cursor = Cursor::new(&wire[..]);
        assert_eq!(
            read_frame_resp(&mut cursor).unwrap().unwrap(),
            b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n"
        );
        assert_eq!(read_frame_resp(&mut cursor).unwrap().unwrap(), b"+OK\r\n");
        assert_eq!(read_frame_resp(&mut cursor).unwrap(), None);
    }

    #[test_log::test]
    fn empty_stream_is_clean_eof() {
        assert_eq!(read_frame_resp(&mut Cursor::new(b"")).unwrap(), None);
    }

    #[test_log::test]
    fn bulk_string_may_contain_crlf() {
        let wire = b"$4\r\na\r\nb\r\n";
        assert_eq!(frame(wire), wire);
        assert_eq!(
            parse_resp(wire).unwrap(),
            RespValue::BulkString(Some(b"a\r\nb"))
        );
    }

    #[test_log::test]
    fn truncated_frame_is_an_error_not_none() {
        let wire = b"*2\r\n$3\r\nGET\r\n$1\r\nk\r\n";
        // Cutting inside a line is a "not CRLF-terminated" protocol error;
        // cutting between lines is EOF. Either way, never Ok.
        for prefix_len in 1..wire.len() {
            let r = read_frame_resp(&mut Cursor::new(&wire[..prefix_len]));
            assert!(r.is_err(), "prefix {prefix_len} accepted: {r:?}");
        }
        assert_eof(read_frame_resp(&mut Cursor::new(b"*1\r\n")));
        assert_eof(read_frame_resp(&mut Cursor::new(b"$3\r\nab")));
    }

    #[test_log::test]
    fn oversized_lengths_are_rejected_before_allocating() {
        let wire = format!("${}\r\n", MAX_PAYLOAD + 1);
        assert_protocol_error(read_frame_resp(&mut Cursor::new(wire.as_bytes())));
        let wire = format!("*{}\r\n", MAX_ARRAY_LEN + 1);
        assert_protocol_error(read_frame_resp(&mut Cursor::new(wire.as_bytes())));
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"$99999999999\r\n")));
    }

    #[test_log::test]
    fn malformed_frames_are_protocol_errors() {
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"*1\n")));
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"$x\r\n")));
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"$-2\r\n")));
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"$2\r\nabcd\r\n")));
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"?\r\n")));
        // Inline commands (`GET k\r\n`, what `nc`/telnet users type) are unsupported.
        assert_protocol_error(read_frame_resp(&mut Cursor::new(b"GET k\r\n")));
    }

    #[test_log::test]
    fn deep_nesting_is_framed_but_not_parsed() {
        let mut wire = b"*1\r\n".repeat(MAX_DEPTH + 2);
        wire.extend_from_slice(b"+x\r\n");
        // The reader has no stack to overflow, so it takes the whole frame...
        assert_eq!(frame(&wire), wire);
        // ...and the recursive parser refuses it.
        assert_protocol_error(parse_resp(&wire));
    }

    #[test_log::test]
    fn nested_arrays_are_framed_iteratively() {
        let wire = b"*2\r\n*2\r\n:1\r\n*0\r\n$1\r\nx\r\n+tail\r\n";
        let mut cursor = Cursor::new(&wire[..]);
        assert_eq!(
            read_frame_resp(&mut cursor).unwrap().unwrap(),
            b"*2\r\n*2\r\n:1\r\n*0\r\n$1\r\nx\r\n"
        );
        assert_eq!(read_frame_resp(&mut cursor).unwrap().unwrap(), b"+tail\r\n");
    }

    // ---------- parsing ----------

    #[test_log::test]
    fn parses_every_type() {
        assert_eq!(
            parse_resp(b"+OK\r\n").unwrap(),
            RespValue::SimpleString(b"OK")
        );
        assert_eq!(
            parse_resp(b"-ERR x\r\n").unwrap(),
            RespValue::Error(b"ERR x")
        );
        assert_eq!(parse_resp(b":-42\r\n").unwrap(), RespValue::Integer(-42));
        assert_eq!(parse_resp(b"$-1\r\n").unwrap(), RespValue::BulkString(None));
        assert_eq!(
            parse_resp(b"$0\r\n\r\n").unwrap(),
            RespValue::BulkString(Some(b""))
        );
        assert_eq!(parse_resp(b"*-1\r\n").unwrap(), RespValue::Array(None));
        assert_eq!(
            parse_resp(b"*0\r\n").unwrap(),
            RespValue::Array(Some(vec![]))
        );
        assert_eq!(
            parse_resp(b"*2\r\n$3\r\nGET\r\n:1\r\n").unwrap(),
            RespValue::Array(Some(vec![
                RespValue::BulkString(Some(b"GET")),
                RespValue::Integer(1),
            ]))
        );
    }

    #[test_log::test]
    fn parse_rejects_trailing_bytes_and_short_input() {
        assert_protocol_error(parse_resp(b"+OK\r\n+OK\r\n"));
        assert_protocol_error(parse_resp(b"$3\r\nab\r\n"));
        assert_protocol_error(parse_resp(b"*2\r\n+a\r\n"));
        assert_protocol_error(parse_resp(b":abc\r\n"));
    }

    // ---------- requests ----------

    fn kv(wire: &[u8]) -> Request<'_> {
        match decode_request_resp(wire).unwrap() {
            RespRequest::Kv(req) => req,
            other => panic!("expected Kv, got {other:?}"),
        }
    }

    #[test_log::test]
    fn decodes_commands_case_insensitively() {
        assert_eq!(
            kv(b"*2\r\n$3\r\nget\r\n$1\r\nk\r\n"),
            Request::Get { key: "k" }
        );
        assert_eq!(
            kv(b"*3\r\n$3\r\nSeT\r\n$1\r\nk\r\n$2\r\nvv\r\n"),
            Request::Set {
                key: "k",
                value: "vv"
            }
        );
        assert_eq!(
            kv(b"*2\r\n$3\r\nDEL\r\n$1\r\na\r\n"),
            Request::Rm { key: "a" }
        );
    }

    #[test_log::test]
    fn decodes_config_get_without_touching_the_store() {
        assert_eq!(
            decode_request_resp(b"*3\r\n$6\r\nCONFIG\r\n$3\r\nGET\r\n$4\r\nsave\r\n").unwrap(),
            RespRequest::ConfigGet(vec![b"save"])
        );
        assert_eq!(
            decode_request_resp(b"*4\r\n$6\r\nconfig\r\n$3\r\nget\r\n$1\r\na\r\n$1\r\nb\r\n")
                .unwrap(),
            RespRequest::ConfigGet(vec![b"a", b"b"])
        );
    }

    #[test_log::test]
    fn config_get_replies_with_name_value_pairs() {
        let mut out = Vec::new();
        write_config_get(&mut out, &[b"save"]).unwrap();
        assert_eq!(out, b"*2\r\n$4\r\nsave\r\n$0\r\n\r\n");

        let mut out = Vec::new();
        write_config_get(&mut out, &[b"a", b"bb"]).unwrap();
        assert_eq!(out, b"*4\r\n$1\r\na\r\n$0\r\n\r\n$2\r\nbb\r\n$0\r\n\r\n");

        let mut out = Vec::new();
        write_config_get(&mut out, &[]).unwrap();
        assert_eq!(out, b"*0\r\n");
    }

    #[test_log::test]
    fn decodes_empty_and_unicode_arguments() {
        assert_eq!(
            kv(b"*3\r\n$3\r\nSET\r\n$0\r\n\r\n$0\r\n\r\n"),
            Request::Set { key: "", value: "" }
        );
        let wire = "*3\r\n$3\r\nSET\r\n$8\r\nключ\r\n$4\r\n🚀\r\n".as_bytes();
        assert_eq!(
            kv(wire),
            Request::Set {
                key: "ключ",
                value: "🚀"
            }
        );
    }

    #[test_log::test]
    fn bad_commands_carry_redis_error_text() {
        let msg = |wire: &[u8]| decode_error_message(&decode_request_resp(wire).unwrap_err());
        assert_eq!(msg(b"*1\r\n$4\r\nNOPE\r\n"), "unknown command 'NOPE'");
        assert_eq!(
            msg(b"*1\r\n$3\r\nGET\r\n"),
            "wrong number of arguments for 'get' command"
        );
        assert_eq!(
            msg(b"*2\r\n$3\r\nSET\r\n$1\r\nk\r\n"),
            "wrong number of arguments for 'set' command"
        );
        assert_eq!(
            msg(b"*3\r\n$3\r\nDEL\r\n$1\r\na\r\n$1\r\nb\r\n"),
            "wrong number of arguments for 'del' command"
        );
        assert_eq!(msg(b"*2\r\n$3\r\nGET\r\n$1\r\n\xff\r\n"), "invalid utf-8");
        assert_eq!(
            msg(b"*3\r\n$6\r\nCONFIG\r\n$3\r\nSET\r\n$1\r\nx\r\n"),
            "unknown subcommand for 'config' command"
        );
    }

    #[test_log::test]
    fn request_must_be_array_of_bulk_strings() {
        assert_protocol_error(decode_request_resp(b"+GET\r\n"));
        assert_protocol_error(decode_request_resp(b"*0\r\n"));
        assert_protocol_error(decode_request_resp(b"*-1\r\n"));
        assert_protocol_error(decode_request_resp(b"*1\r\n:1\r\n"));
        assert_protocol_error(decode_request_resp(b"*2\r\n$3\r\nGET\r\n$-1\r\n"));
    }

    // ---------- responses ----------

    #[test_log::test]
    fn response_encoding_follows_redis_semantics() {
        use RespCommand::*;
        let not_found = Response::Error(ErrorCode::KeyNotFound);
        let internal = Response::Error(ErrorCode::Internal);

        assert_eq!(encoded(Get, &Response::Value("v".into())), b"$1\r\nv\r\n");
        assert_eq!(encoded(Get, &Response::Value(String::new())), b"$0\r\n\r\n");
        assert_eq!(
            encoded(Get, &Response::Value("a\r\nb".into())),
            b"$4\r\na\r\nb\r\n"
        );
        assert_eq!(encoded(Get, &not_found), b"$-1\r\n");

        assert_eq!(encoded(Set, &Response::Ok), b"+OK\r\n");

        assert_eq!(encoded(Del, &Response::Ok), b":1\r\n");
        assert_eq!(encoded(Del, &not_found), b":0\r\n");

        for cmd in [Get, Set, Del] {
            assert_eq!(encoded(cmd, &internal), b"-ERR Internal server error\r\n");
        }
    }

    #[test_log::test]
    fn error_message_cannot_break_framing() {
        let mut out = Vec::new();
        write_error(&mut out, "bad\r\nthing").unwrap();
        assert_eq!(out, b"-ERR bad  thing\r\n");
    }

    #[test_log::test]
    fn responses_parse_back() {
        for (cmd, resp) in [
            (RespCommand::Set, Response::Ok),
            (RespCommand::Del, Response::Ok),
            (RespCommand::Get, Response::Value("x\r\ny".into())),
            (RespCommand::Get, Response::Error(ErrorCode::KeyNotFound)),
            (RespCommand::Del, Response::Error(ErrorCode::KeyNotFound)),
            (RespCommand::Get, Response::Error(ErrorCode::Internal)),
        ] {
            let wire = encoded(cmd, &resp);
            assert_eq!(frame(&wire), wire);
            parse_resp(&wire).unwrap();
        }
    }

    // ---------- full cycle ----------

    #[derive(Clone)]
    struct FakeStore(std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>);

    impl KvStore for FakeStore {
        fn open(_: crate::Config) -> Result<Self> {
            Ok(Self(Default::default()))
        }
        fn set(&self, key: &str, value: &str) -> Result<()> {
            self.0.lock().unwrap().insert(key.into(), value.into());
            Ok(())
        }
        fn remove(&self, key: &str) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .remove(key)
                .map(|_| ())
                .ok_or(KvsError::KeyNotFound)
        }
        fn get(&self, key: &str) -> Result<Option<String>> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }
    }

    #[test_log::test]
    fn pipelined_session_over_a_byte_stream() {
        let store = FakeStore(Default::default());
        let inbound: &[u8] = b"*3\r\n$6\r\nCONFIG\r\n$3\r\nGET\r\n$4\r\nsave\r\n\
                              *3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n\
                              *2\r\n$3\r\nGET\r\n$1\r\na\r\n\
                              *2\r\n$3\r\nGET\r\n$7\r\nmissing\r\n\
                              *1\r\n$5\r\nBOGUS\r\n\
                              *2\r\n$3\r\nDEL\r\n$1\r\na\r\n\
                              *2\r\n$3\r\nDEL\r\n$1\r\na\r\n";

        // Same loop as kvs-server::handle_resp.
        let mut reader = Cursor::new(inbound);
        let mut outbound = Vec::new();
        while let Some(frame) = read_frame_resp(&mut reader).unwrap() {
            match decode_request_resp(&frame) {
                Ok(RespRequest::Kv(req)) => {
                    let cmd = RespCommand::from(&req);
                    let resp = execute(&store, req);
                    write_response_resp(&mut outbound, cmd, &resp).unwrap();
                }
                Ok(RespRequest::ConfigGet(names)) => {
                    write_config_get(&mut outbound, &names).unwrap()
                }
                Err(e) => write_error(&mut outbound, &decode_error_message(&e)).unwrap(),
            }
        }
        assert_eq!(
            outbound,
            b"*2\r\n$4\r\nsave\r\n$0\r\n\r\n\
              +OK\r\n$1\r\n1\r\n$-1\r\n-ERR unknown command 'BOGUS'\r\n:1\r\n:0\r\n"
        );
    }
}
