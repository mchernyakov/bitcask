use crate::command::{Command, HEADER_LEN};
use crate::Result;
use std::io;
use std::io::Read;
use tracing::debug;

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
    // TODO return a reference?
    pub fn new(reader: R, file_len: u64) -> Self {
        RecordReader {
            reader,
            position: 0,
            file_len,
        }
    }

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
