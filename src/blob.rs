//! Read and decode blobs

use crate::block::{HeaderBlock, PrimitiveBlock};
use crate::error::{BlobError, Error, ErrorKind, Result, new_blob_error, new_error};
use crate::wire::{FieldIter, WireValue};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::Path;

use flate2::read::ZlibDecoder;

/// Maximum allowed [`BlobHeader`] size in bytes.
pub const MAX_BLOB_HEADER_SIZE: u64 = 64 * 1024;

/// Maximum allowed uncompressed [`Blob`] content size in bytes.
///
/// The format applies the same limit to the size of the serialized `Blob`
/// message, which is what the `datasize` field of a blob header announces.
pub const MAX_BLOB_MESSAGE_SIZE: u64 = 32 * 1024 * 1024;

/// The content type of a blob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobType<'a> {
    /// Blob contains a [`HeaderBlock`].
    OsmHeader,
    /// Blob contains a [`PrimitiveBlock`].
    OsmData,
    /// An unknown blob type with the given string identifier.
    /// Parsers should ignore unknown blobs they do not expect.
    Unknown(&'a str),
}

impl<'a> BlobType<'a> {
    /// Returns the identifier as it appears in the file.
    pub const fn as_str(&self) -> &'a str {
        match self {
            Self::OsmHeader => "OSMHeader",
            Self::OsmData => "OSMData",
            Self::Unknown(x) => x,
        }
    }
}

/// The decoded content of a blob (analogous to [`BlobType`]).
#[derive(Clone, Debug)]
pub enum BlobDecode<'a> {
    /// Blob contains a [`HeaderBlock`].
    OsmHeader(Box<HeaderBlock>),
    /// Blob contains a [`PrimitiveBlock`].
    OsmData(PrimitiveBlock),
    /// An unknown blob type with the given string identifier.
    /// Parsers should ignore unknown blobs they do not expect.
    Unknown(&'a str),
}

/// The offset of a blob in bytes from stream start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteOffset(pub u64);

/// Where the payload of a blob lives, and how it is compressed.
#[derive(Clone, Debug)]
enum Payload {
    /// Uncompressed bytes at the given range of the blob message.
    Raw(Range<usize>),
    /// zlib compressed bytes at the given range.
    Zlib(Range<usize>),
    /// A compression format the format defines but this crate does not decode.
    Unsupported(&'static str),
    /// Neither `raw` nor a compression field is set.
    Missing,
}

/// A blob.
///
/// A PBF file consists of a sequence of blobs. This type supports decoding the content of a blob
/// to different types of blocks that are usually more interesting to the user.
#[derive(Clone, Debug)]
pub struct Blob {
    /// The serialized `Blob` message.
    data: Vec<u8>,
    /// Where the payload is inside `data`.
    payload: Payload,
    /// The type string taken from the preceding blob header.
    type_: String,
    offset: Option<ByteOffset>,
}

impl Blob {
    /// Reads a blob message, recording where its payload lives.
    fn from_parts(type_: String, data: Vec<u8>, offset: Option<ByteOffset>) -> Result<Blob> {
        let mut payload = Payload::Missing;

        let mut fields = FieldIter::new(&data);
        while let Some((field, value)) = fields.next_field()? {
            if let WireValue::Bytes(bytes) = value {
                let start = bytes.as_ptr() as usize - data.as_ptr() as usize;
                let range = start..start + bytes.len();
                match field {
                    1 => payload = Payload::Raw(range),
                    3 => payload = Payload::Zlib(range),
                    4 => payload = Payload::Unsupported("lzma"),
                    6 => payload = Payload::Unsupported("lz4"),
                    7 => payload = Payload::Unsupported("zstd"),
                    _ => {}
                }
            }
        }

        Ok(Blob {
            data,
            payload,
            type_,
            offset,
        })
    }

    /// Decodes the Blob and tries to obtain the inner content (usually a [`HeaderBlock`] or a
    /// [`PrimitiveBlock`]). This operation might involve an expensive decompression step.
    pub fn decode(&self) -> Result<BlobDecode<'_>> {
        match self.get_type() {
            BlobType::OsmHeader => {
                let block = Box::new(self.to_headerblock()?);
                Ok(BlobDecode::OsmHeader(block))
            }
            BlobType::OsmData => {
                let block = self.to_primitiveblock()?;
                Ok(BlobDecode::OsmData(block))
            }
            BlobType::Unknown(x) => Ok(BlobDecode::Unknown(x)),
        }
    }

    /// Returns the type of a blob without decoding its content.
    pub fn get_type(&self) -> BlobType<'_> {
        match self.type_.as_str() {
            x if x == BlobType::OsmHeader.as_str() => BlobType::OsmHeader,
            x if x == BlobType::OsmData.as_str() => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the byte offset of the blob from the start of its source stream.
    /// This might be [`None`] if the source stream does not implement [`Seek`].
    pub fn offset(&self) -> Option<ByteOffset> {
        self.offset
    }

    /// Tries to decode the blob to a [`HeaderBlock`]. This operation might involve an expensive
    /// decompression step.
    pub fn to_headerblock(&self) -> Result<HeaderBlock> {
        let payload = self.decompressed()?;
        HeaderBlock::parse(&payload)
    }

    /// Tries to decode the blob to a [`PrimitiveBlock`]. This operation might involve an expensive
    /// decompression step.
    pub fn to_primitiveblock(&self) -> Result<PrimitiveBlock> {
        let payload = self.decompressed()?;
        PrimitiveBlock::parse(payload)
    }

    /// Returns the uncompressed content of the blob.
    ///
    /// The declared size limits are enforced here: an oversized or unsupported
    /// blob is reported instead of being handed to a parser.
    fn decompressed(&self) -> Result<Vec<u8>> {
        match &self.payload {
            Payload::Raw(range) => {
                let payload = &self.data[range.clone()];
                let size = payload.len() as u64;
                if size < MAX_BLOB_MESSAGE_SIZE {
                    Ok(payload.to_vec())
                } else {
                    Err(new_blob_error(BlobError::MessageTooBig { size }))
                }
            }
            Payload::Zlib(range) => {
                let mut decoder =
                    ZlibDecoder::new(&self.data[range.clone()]).take(MAX_BLOB_MESSAGE_SIZE);
                let mut buffer = Vec::new();
                decoder.read_to_end(&mut buffer).map_err(Error::from)?;
                Ok(buffer)
            }
            // Recognized by the format, but not implemented here. Reporting
            // the format explicitly beats claiming that the blob is empty.
            Payload::Unsupported(kind) => {
                Err(new_blob_error(BlobError::UnsupportedCompression { kind }))
            }
            Payload::Missing => Err(new_blob_error(BlobError::Empty)),
        }
    }
}

/// A blob header.
///
/// Just contains information about the size and type of the following [`Blob`].
#[derive(Clone, Debug)]
pub struct BlobHeader {
    type_: String,
    datasize: i32,
}

impl BlobHeader {
    /// Parses a serialized `BlobHeader` message.
    fn parse(data: &[u8]) -> Result<BlobHeader> {
        let mut type_ = None;
        let mut datasize = None;

        let mut fields = FieldIter::new(data);
        while let Some((field, value)) = fields.next_field()? {
            match (field, value) {
                (1, WireValue::Bytes(bytes)) => {
                    let text = std::str::from_utf8(bytes).map_err(|_| {
                        new_blob_error(BlobError::MalformedHeader {
                            reason: "blob type is not valid UTF-8",
                        })
                    })?;
                    type_ = Some(text.to_owned());
                }
                (3, WireValue::Varint(value)) => datasize = Some(value as i32),
                _ => {}
            }
        }

        match (type_, datasize) {
            (Some(type_), Some(datasize)) => Ok(BlobHeader { type_, datasize }),
            _ => Err(new_blob_error(BlobError::MalformedHeader {
                reason: "blob header is missing a required field",
            })),
        }
    }

    /// Returns the type of the following blob.
    pub fn blob_type(&self) -> BlobType<'_> {
        match self.type_.as_str() {
            "OSMHeader" => BlobType::OsmHeader,
            "OSMData" => BlobType::OsmData,
            x => BlobType::Unknown(x),
        }
    }

    /// Returns the size of the following blob in bytes.
    pub fn get_blob_size(&self) -> i32 {
        self.datasize
    }
}

/// A reader for PBF files that allows iterating over [`Blob`]s.
#[derive(Clone, Debug)]
pub struct BlobReader<R: Read + Send> {
    reader: R,
    /// Current reader offset in bytes from the start of the stream.
    offset: Option<ByteOffset>,
    last_blob_ok: bool,
}

impl<R: Read + Send> BlobReader<R> {
    /// Creates a new `BlobReader`.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let reader = BlobReader::new(buf_reader);
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new(reader: R) -> BlobReader<R> {
        BlobReader {
            reader,
            offset: None,
            last_blob_ok: true,
        }
    }

    /// Reads the length prefix of the next blob header.
    ///
    /// Returns `None` at a clean end of the stream, i.e. when the stream ends
    /// exactly on a blob boundary. A partial length prefix (1 to 3 bytes) is a
    /// truncated file and reported as an error.
    fn read_header_size(&mut self) -> Option<Result<u64>> {
        let mut size_bytes = [0_u8; 4];
        let mut filled = 0;

        while filled < size_bytes.len() {
            match self.reader.read(&mut size_bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == ::std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.offset = None;
                    self.last_blob_ok = false;
                    return Some(Err(e.into()));
                }
            }
        }

        match filled {
            0 => None,
            1..=3 => {
                self.offset = None;
                self.last_blob_ok = false;
                Some(Err(new_blob_error(BlobError::InvalidHeaderSize)))
            }
            _ => {
                self.offset = self.offset.map(|x| ByteOffset(x.0 + 4));
                Some(Ok(u64::from(u32::from_be_bytes(size_bytes))))
            }
        }
    }

    /// Reads exactly `len` bytes, or reports that the stream ended early.
    fn read_exact_bytes(&mut self, len: u64) -> Result<Vec<u8>> {
        let mut buffer = Vec::with_capacity(len as usize);
        let mut reader = self.reader.by_ref().take(len);
        reader.read_to_end(&mut buffer).map_err(Error::from)?;
        if buffer.len() as u64 != len {
            return Err(truncated_stream("content too short"));
        }
        Ok(buffer)
    }

    fn read_blob_header(&mut self) -> Option<Result<BlobHeader>> {
        let header_size = match self.read_header_size() {
            Some(Ok(size)) => size,
            Some(Err(err)) => return Some(Err(err)),
            None => return None,
        };

        if header_size >= MAX_BLOB_HEADER_SIZE {
            self.last_blob_ok = false;
            return Some(Err(new_blob_error(BlobError::HeaderTooBig {
                size: header_size,
            })));
        }

        let data = match self.read_exact_bytes(header_size) {
            Ok(data) => data,
            Err(err) => {
                self.offset = None;
                self.last_blob_ok = false;
                return Some(Err(err));
            }
        };

        let header = match BlobHeader::parse(&data) {
            Ok(header) => header,
            Err(err) => {
                self.offset = None;
                self.last_blob_ok = false;
                return Some(Err(err));
            }
        };

        self.offset = self.offset.map(|x| ByteOffset(x.0 + header_size));

        Some(Ok(header))
    }
}

impl BlobReader<BufReader<File>> {
    /// Tries to open the file at the given path and constructs a `BlobReader` from this.
    /// If there are no errors, each blob will have a valid ([`Some`]) offset.
    ///
    /// # Errors
    /// Returns the same errors that `std::fs::File::open` returns.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = BlobReader::from_path("tests/test.osm.pbf")?;
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let f = File::open(path)?;
        let reader = BufReader::new(f);

        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(0)),
            last_blob_ok: true,
        })
    }
}

impl<R: Read + Send> Iterator for BlobReader<R> {
    type Item = Result<Blob>;

    fn next(&mut self) -> Option<Self::Item> {
        // Stop iteration if there was an error.
        if !self.last_blob_ok {
            return None;
        }

        let prev_offset = self.offset;

        let header = match self.read_blob_header() {
            Some(Ok(header)) => header,
            Some(Err(err)) => return Some(Err(err)),
            None => return None,
        };

        let data_size = match blob_data_size(&header) {
            Ok(size) => size,
            Err(err) => {
                self.last_blob_ok = false;
                return Some(Err(err));
            }
        };

        let data = match self.read_exact_bytes(data_size) {
            Ok(data) => data,
            Err(err) => {
                self.offset = None;
                self.last_blob_ok = false;
                return Some(Err(err));
            }
        };

        self.offset = self.offset.map(|x| ByteOffset(x.0 + data_size));

        let blob = match Blob::from_parts(header.type_.clone(), data, prev_offset) {
            Ok(blob) => blob,
            Err(err) => {
                self.offset = None;
                self.last_blob_ok = false;
                return Some(Err(err));
            }
        };

        Some(Ok(blob))
    }
}

impl<R: Read + Seek + Send> BlobReader<R> {
    /// Creates a new `BlobReader` from the given reader that is seekable and will be initialized
    /// with a valid offset.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let f = std::fs::File::open("tests/test.osm.pbf")?;
    /// let buf_reader = std::io::BufReader::new(f);
    ///
    /// let mut reader = BlobReader::new_seekable(buf_reader)?;
    /// let first_blob = reader.next().unwrap()?;
    ///
    /// assert_eq!(first_blob.offset(), Some(ByteOffset(0)));
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn new_seekable(mut reader: R) -> Result<BlobReader<R>> {
        let pos = reader.stream_position()?;

        Ok(BlobReader {
            reader,
            offset: Some(ByteOffset(pos)),
            last_blob_ok: true,
        })
    }

    /// Read and return the [`Blob`] at the given offset. If successful, the cursor of the stream is
    /// positioned at the start of the next [`Blob`].
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = BlobReader::from_path("tests/test.osm.pbf")?;
    /// let first_blob = reader.next().unwrap()?;
    /// let second_blob = reader.next().unwrap()?;
    ///
    /// let offset = first_blob.offset().unwrap();
    /// let first_blob_again = reader.blob_from_offset(offset)?;
    /// assert_eq!(first_blob.offset(), first_blob_again.offset());
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn blob_from_offset(&mut self, pos: ByteOffset) -> Result<Blob> {
        self.seek(pos)?;
        self.next().unwrap_or_else(|| {
            Err(new_error(ErrorKind::Io(::std::io::Error::new(
                ::std::io::ErrorKind::UnexpectedEof,
                "no blob at this stream position",
            ))))
        })
    }

    /// Seek to an offset in bytes from the start of the stream.
    ///
    /// Seeking also clears a previous decoding error, so a reader that stopped
    /// on a malformed blob can be repositioned to a known-good offset and
    /// reused.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = BlobReader::from_path("tests/test.osm.pbf")?;
    /// let first_blob = reader.next().unwrap()?;
    /// let second_blob = reader.next().unwrap()?;
    ///
    /// reader.seek(first_blob.offset().unwrap())?;
    ///
    /// let first_blob_again = reader.next().unwrap()?;
    /// assert_eq!(first_blob.offset(), first_blob_again.offset());
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn seek(&mut self, pos: ByteOffset) -> Result<()> {
        match self.reader.seek(SeekFrom::Start(pos.0)) {
            Ok(offset) => {
                self.offset = Some(ByteOffset(offset));
                self.last_blob_ok = true;
                Ok(())
            }
            Err(e) => {
                self.offset = None;
                Err(e.into())
            }
        }
    }

    /// Seek to an offset in bytes. (See `std::io::Seek`)
    ///
    /// Like [`seek`](Self::seek), this clears a previous decoding error.
    pub fn seek_raw(&mut self, pos: SeekFrom) -> Result<u64> {
        match self.reader.seek(pos) {
            Ok(offset) => {
                self.offset = Some(ByteOffset(offset));
                self.last_blob_ok = true;
                Ok(offset)
            }
            Err(e) => {
                self.offset = None;
                Err(e.into())
            }
        }
    }

    /// Read and return next [`BlobHeader`] but skip the following [`Blob`]. This allows really fast
    /// iteration of the PBF structure if only the byte offset and [`BlobType`] are important.
    /// On success, returns the [`BlobHeader`] and the byte offset of the header which can also be
    /// used as an offset for reading the entire [`Blob`] (including header).
    pub fn next_header_skip_blob(&mut self) -> Option<Result<(BlobHeader, Option<ByteOffset>)>> {
        // Stop iteration if there was an error.
        if !self.last_blob_ok {
            return None;
        }

        let prev_offset = self.offset;

        // read header
        let header = match self.read_blob_header() {
            Some(Ok(header)) => header,
            Some(Err(err)) => return Some(Err(err)),
            None => return None,
        };

        let data_size = match blob_data_size(&header) {
            Ok(size) => size,
            Err(err) => {
                self.last_blob_ok = false;
                return Some(Err(err));
            }
        };

        // skip blob (which also adjusts self.offset)
        if let Err(err) = self.seek_raw(SeekFrom::Current(data_size as i64)) {
            self.last_blob_ok = false;
            return Some(Err(err));
        }

        Some(Ok((header, prev_offset)))
    }
}

impl BlobReader<BufReader<File>> {
    /// Creates a new `BlobReader` from the given path that is seekable and will be initialized
    /// with a valid offset.
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let mut reader = BlobReader::seekable_from_path("tests/test.osm.pbf")?;
    /// let first_blob = reader.next().unwrap()?;
    ///
    /// assert_eq!(first_blob.offset(), Some(ByteOffset(0)));
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn seekable_from_path<P: AsRef<Path>>(path: P) -> Result<BlobReader<BufReader<File>>> {
        let f = File::open(path.as_ref())?;
        let buf_reader = BufReader::new(f);
        Self::new_seekable(buf_reader)
    }
}

/// Builds an `UnexpectedEof` error for a stream that ends inside a blob.
fn truncated_stream(message: &'static str) -> Error {
    new_error(ErrorKind::Io(::std::io::Error::new(
        ::std::io::ErrorKind::UnexpectedEof,
        message,
    )))
}

/// Validates the `datasize` field of a blob header.
///
/// The format defines `datasize` as the exact length of the serialized `Blob`
/// message, which is capped by [`MAX_BLOB_MESSAGE_SIZE`]. A negative or
/// oversized value means the file is corrupt: computing offsets from it would
/// wrap around and could point anywhere in the stream.
pub(crate) fn blob_data_size(header: &BlobHeader) -> Result<u64> {
    let size = header.datasize;
    if (0..=MAX_BLOB_MESSAGE_SIZE as i32).contains(&size) {
        Ok(size as u64)
    } else {
        Err(new_blob_error(BlobError::InvalidDataSize {
            size: i64::from(size),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_type() {
        let pairs = [
            ("", BlobType::Unknown("")),
            ("abc", BlobType::Unknown("abc")),
            ("OSMHeader", BlobType::OsmHeader),
            ("OSMData", BlobType::OsmData),
        ];

        for (string, blob_type) in &pairs {
            let blob = Blob::from_parts((*string).to_owned(), Vec::new(), None).unwrap();
            assert_eq!(blob.get_type(), *blob_type);
        }
    }
}
