use crate::Result;
use crate::command::{Command, HEADER_LEN};
use std::io;
use std::io::Read;
use tracing::debug;

#[derive(Debug)]
pub enum RecordRead {
    Record { position: u64, bytes: Vec<u8> },
    CleanEof,
    TornTail { position: u64 }, // record runs past end-of-file
}

pub struct RecordReader<R: Read> {
    reader: R,
    position: u64,
    file_len: u64,
}

impl<R: Read> RecordReader<R> {
    pub fn new(reader: R, file_len: u64) -> Self {
        RecordReader {
            reader,
            position: 0,
            file_len,
        }
    }

    // perf idea: keep a reusable internal buffer
    // and lend `&[u8]` into it (`RecordRead<'_>`) instead of allocating a
    // Vec per record (the lending pattern)
    pub fn next_record(&mut self) -> Result<RecordRead> {
        let start = self.position;
        let mut header = [0u8; HEADER_LEN];
        match self.reader.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return if start == self.file_len {
                    Ok(RecordRead::CleanEof)
                } else {
                    Ok(RecordRead::TornTail { position: start })
                };
            }
            Err(e) => return Err(e.into()),
        }

        let body_len = Command::body_len(&header);

        let remaining = self.file_len - start - HEADER_LEN as u64;
        if body_len as u64 > remaining {
            debug!(
                "record is torn, remaining: {}, body_len: {}",
                remaining, body_len
            );
            return Ok(RecordRead::TornTail { position: start });
        }

        let mut buf = vec![0u8; HEADER_LEN + body_len];
        buf[..HEADER_LEN].copy_from_slice(&header);

        self.position += HEADER_LEN as u64 + body_len as u64;

        match self.reader.read_exact(&mut buf[HEADER_LEN..]) {
            Ok(()) => {
                self.position = start + HEADER_LEN as u64 + body_len as u64;
                Ok(RecordRead::Record {
                    position: start,
                    bytes: buf,
                })
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                Ok(RecordRead::TornTail { position: start })
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn record(key: &[u8], value: &[u8]) -> Vec<u8> {
        Command::Set { ts: 1, key, value }.serialize()
    }

    #[track_caller]
    fn expect_record(read: RecordRead) -> (u64, Vec<u8>) {
        match read {
            RecordRead::Record { position, bytes } => (position, bytes),
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn yields_records_then_clean_eof() -> Result<()> {
        let first = record(b"alpha", b"beta");
        let second = record(b"gamma", b"delta");
        let mut buf = first.clone();
        buf.extend_from_slice(&second);

        let mut reader = RecordReader::new(Cursor::new(&buf), buf.len() as u64);

        let (position, bytes) = expect_record(reader.next_record()?);
        assert_eq!(position, 0);
        assert_eq!(bytes, first);

        let (position, bytes) = expect_record(reader.next_record()?);
        assert_eq!(position, first.len() as u64);
        assert_eq!(bytes, second);

        assert!(matches!(reader.next_record()?, RecordRead::CleanEof));
        // CleanEof is stable: asking again must not turn into TornTail
        assert!(matches!(reader.next_record()?, RecordRead::CleanEof));

        Ok(())
    }

    #[test]
    fn partial_header_at_tail_is_torn() -> Result<()> {
        let first = record(b"alpha", b"beta");
        let mut buf = first.clone();
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC]);

        let mut reader = RecordReader::new(Cursor::new(&buf), buf.len() as u64);

        expect_record(reader.next_record()?);
        match reader.next_record()? {
            RecordRead::TornTail { position } => assert_eq!(position, first.len() as u64),
            other => panic!("expected TornTail, got {other:?}"),
        }

        Ok(())
    }

    #[test]
    fn oversized_body_len_is_torn_tail() -> Result<()> {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[4..8].copy_from_slice(&u32::MAX.to_le_bytes());

        let mut reader = RecordReader::new(Cursor::new(&buf), buf.len() as u64);

        assert!(matches!(
            reader.next_record()?,
            RecordRead::TornTail { position: 0 }
        ));

        Ok(())
    }

    // file_len can lie (e.g. the file shrank between metadata() and the read):
    // a body that ends early must surface as TornTail, never a panic.
    #[test]
    fn body_shorter_than_file_len_claims_is_torn() -> Result<()> {
        let full = record(b"alpha", b"beta");
        let truncated = &full[..HEADER_LEN + 2];

        let mut reader = RecordReader::new(Cursor::new(truncated), full.len() as u64);

        assert!(matches!(
            reader.next_record()?,
            RecordRead::TornTail { position: 0 }
        ));

        Ok(())
    }
}
