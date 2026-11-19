//! Minimal protobuf wire-format reading for the block hot path.
//!
//! Only the parts the format actually uses are implemented: varints,
//! length-delimited fields and packed arrays. Unknown fields are skipped
//! instead of rejected, which is what keeps well-formed future files readable.

use crate::error::{ErrorKind, Result, new_error};

/// Builds an error for a block that does not follow the wire format.
fn malformed(reason: &'static str) -> crate::error::Error {
    new_error(ErrorKind::MalformedBlock { reason })
}

/// Reads one base-128 varint, advancing `pos`.
pub(crate) fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0_u64;
    let mut shift = 0;
    while shift < 64 {
        let byte = match buf.get(*pos) {
            Some(byte) => *byte,
            None => return Err(malformed("truncated varint")),
        };
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
    Err(malformed("varint is longer than 64 bits"))
}

/// Reads one varint, returning `None` when the buffer ends or the value would
/// overflow. Used on the hot path, where allocating an error would dominate.
#[inline]
pub(crate) fn read_varint_opt(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let first = *buf.get(*pos)?;
    *pos += 1;
    if first < 0x80 {
        return Some(u64::from(first));
    }
    let mut value = u64::from(first & 0x7f);
    let mut shift = 7;
    while shift < 64 {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

/// Decodes a zigzag-encoded (`sint32`/`sint64`) value.
pub(crate) fn zigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The value of one field.
pub(crate) enum WireValue<'a> {
    /// A varint field.
    Varint(u64),
    /// A length-delimited field (string, bytes, message or packed array).
    Bytes(&'a [u8]),
}

/// Iterates over the fields of a serialized message.
#[derive(Clone, Debug)]
pub(crate) struct FieldIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> FieldIter<'a> {
    /// Creates an iterator over the fields of one message.
    pub(crate) fn new(buf: &'a [u8]) -> FieldIter<'a> {
        FieldIter { buf, pos: 0 }
    }

    /// Returns the next field, skipping fields whose wire type carries no
    /// value this crate uses (fixed32/fixed64), or `None` at the end.
    pub(crate) fn next_field(&mut self) -> Result<Option<(u64, WireValue<'a>)>> {
        loop {
            if self.pos >= self.buf.len() {
                return Ok(None);
            }
            let tag = read_varint(self.buf, &mut self.pos)?;
            let field = tag >> 3;
            if field == 0 {
                return Err(malformed("field number 0"));
            }
            match tag & 7 {
                0 => {
                    let value = read_varint(self.buf, &mut self.pos)?;
                    return Ok(Some((field, WireValue::Varint(value))));
                }
                1 => self.advance(8)?,
                2 => {
                    let len = read_varint(self.buf, &mut self.pos)? as usize;
                    let start = self.pos;
                    let end = start
                        .checked_add(len)
                        .ok_or_else(|| malformed("field length overflows"))?;
                    if end > self.buf.len() {
                        return Err(malformed("truncated length-delimited field"));
                    }
                    self.pos = end;
                    return Ok(Some((field, WireValue::Bytes(&self.buf[start..end]))));
                }
                5 => self.advance(4)?,
                _ => return Err(malformed("unsupported wire type")),
            }
        }
    }

    /// Skips `len` bytes of a fixed-width field.
    fn advance(&mut self, len: usize) -> Result<()> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| malformed("field length overflows"))?;
        if end > self.buf.len() {
            return Err(malformed("truncated fixed-width field"));
        }
        self.pos = end;
        Ok(())
    }
}

/// How the values of a packed field are encoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Packing {
    /// `int32`/`bool`/`enum` values, stored as two's complement varints.
    Plain,
    /// `sint32`/`sint64` values that are delta coded (coordinates, ids).
    Delta,
    /// `sint32`/`sint64` values that are not delta coded.
    Zigzag,
}

/// A cursor over the values of a packed repeated field.
#[derive(Clone, Debug)]
pub(crate) struct PackedVarints<'a> {
    buf: &'a [u8],
    pos: usize,
    current: i64,
    packing: Packing,
}

impl<'a> PackedVarints<'a> {
    /// Creates a cursor with the given packing.
    pub(crate) fn new(buf: &'a [u8], packing: Packing) -> PackedVarints<'a> {
        PackedVarints {
            buf,
            pos: 0,
            current: 0,
            packing,
        }
    }

    /// Cursor over plain varints.
    pub(crate) fn plain(buf: &'a [u8]) -> PackedVarints<'a> {
        PackedVarints::new(buf, Packing::Plain)
    }

    /// Cursor over delta and zigzag encoded varints.
    pub(crate) fn delta(buf: &'a [u8]) -> PackedVarints<'a> {
        PackedVarints::new(buf, Packing::Delta)
    }

    /// Cursor over zigzag encoded varints that are not accumulated.
    pub(crate) fn zigzag(buf: &'a [u8]) -> PackedVarints<'a> {
        PackedVarints::new(buf, Packing::Zigzag)
    }

    /// Returns the next decoded value, or `None` when the array is exhausted.
    #[inline]
    pub(crate) fn next_value(&mut self) -> Option<i64> {
        let raw = read_varint_opt(self.buf, &mut self.pos)?;
        match self.packing {
            Packing::Plain => Some(raw as i64),
            Packing::Zigzag => Some(zigzag(raw)),
            Packing::Delta => {
                self.current = self.current.wrapping_add(zigzag(raw));
                Some(self.current)
            }
        }
    }

    /// Upper bound for the number of remaining values.
    pub(crate) fn upper_bound(&self) -> usize {
        self.buf.len() - self.pos
    }
}

impl Iterator for PackedVarints<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_value()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.upper_bound()))
    }
}
