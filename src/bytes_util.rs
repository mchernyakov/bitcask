use std::io;

pub struct BytesUtil;

impl BytesUtil {
    #[inline]
    pub fn read_u8(buf: &[u8], cursor: &mut usize) -> io::Result<u8> {
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
    pub fn read_u32(buf: &[u8], cursor: &mut usize) -> io::Result<u32> {
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
    pub fn read_u64(buf: &[u8], cursor: &mut usize) -> io::Result<u64> {
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
    pub fn read_bytes<'a>(buf: &'a [u8], cursor: &mut usize, len: usize) -> io::Result<&'a [u8]> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    #[test]
    fn reads_advance_cursor_in_order() {
        let mut buf = Vec::new();
        buf.push(7u8);
        buf.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        buf.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        buf.extend_from_slice(b"tail");

        let mut cursor = 0;
        assert_eq!(BytesUtil::read_u8(&buf, &mut cursor).unwrap(), 7);
        assert_eq!(BytesUtil::read_u32(&buf, &mut cursor).unwrap(), 0xDEAD_BEEF);
        assert_eq!(
            BytesUtil::read_u64(&buf, &mut cursor).unwrap(),
            0x1122_3344_5566_7788
        );
        assert_eq!(BytesUtil::read_bytes(&buf, &mut cursor, 4).unwrap(), b"tail");
        assert_eq!(cursor, buf.len());
    }

    #[test]
    fn values_are_little_endian() {
        let buf = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

        let mut cursor = 0;
        assert_eq!(BytesUtil::read_u32(&buf, &mut cursor).unwrap(), 0x0403_0201);

        let mut cursor = 0;
        assert_eq!(
            BytesUtil::read_u64(&buf, &mut cursor).unwrap(),
            0x0807_0605_0403_0201
        );
    }

    #[test]
    fn short_buffer_is_eof_and_cursor_is_untouched() {
        let buf = [0u8; 3];

        let mut cursor = 0;
        let err = BytesUtil::read_u32(&buf, &mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(cursor, 0);

        let err = BytesUtil::read_u64(&buf, &mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(cursor, 0);

        let err = BytesUtil::read_bytes(&buf, &mut cursor, 4).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(cursor, 0);

        let mut cursor = buf.len();
        let err = BytesUtil::read_u8(&buf, &mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
        assert_eq!(cursor, buf.len());
    }

    #[test]
    fn zero_length_read_bytes_is_empty() {
        let buf = [1u8, 2];
        let mut cursor = 1;
        assert_eq!(BytesUtil::read_bytes(&buf, &mut cursor, 0).unwrap(), b"");
        assert_eq!(cursor, 1);
    }

    #[test]
    fn overflowing_cursor_is_invalid_data_not_panic() {
        let buf = [0u8; 8];

        let mut cursor = usize::MAX;
        let err = BytesUtil::read_u32(&buf, &mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        let mut cursor = usize::MAX;
        let err = BytesUtil::read_u64(&buf, &mut cursor).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);

        let mut cursor = 1;
        let err = BytesUtil::read_bytes(&buf, &mut cursor, usize::MAX).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }
}
