//! Iterate over the dense nodes in a `PrimitiveGroup`
//!
//! Dense nodes are stored as parallel packed arrays. Instead of materialising
//! those arrays, the iterators here keep one cursor per array and decode the
//! values on demand, so a block costs no per-element allocation.

use crate::block::{get_stringtable_key_value, str_from_stringtable};
use crate::error::Result;
use crate::proto::osmformat;
use crate::wire::{FieldIter, PackedVarints, WireValue, read_varint_opt};

/// An OpenStreetMap node element from a compressed array of dense nodes (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Node)).
#[derive(Clone, Debug)]
pub struct DenseNode<'a> {
    block: &'a osmformat::PrimitiveBlock,

    /// The node id. It should be unique between nodes and might be negative to indicate
    /// that the element has not yet been uploaded to a server.
    pub id: i64,
    lat: i64,
    lon: i64,
    /// The packed (key index, value index) varints of this node's tags.
    keys_vals: &'a [u8],
    info: Option<DenseNodeInfo<'a>>,
}

impl<'a> DenseNode<'a> {
    /// Returns the node id. It should be unique between nodes and might be negative to indicate
    /// that the element has not yet been uploaded to a server.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// return optional metadata about the node
    pub fn info(&'a self) -> Option<&'a DenseNodeInfo<'a>> {
        self.info.as_ref()
    }

    /// Returns the latitude coordinate in degrees.
    pub fn lat(&self) -> f64 {
        1e-9 * self.nano_lat() as f64
    }

    /// Returns the latitude coordinate in nanodegrees (10⁻⁹).
    pub fn nano_lat(&self) -> i64 {
        self.block.lat_offset() + i64::from(self.block.granularity()) * self.lat
    }

    /// Returns the latitude coordinate in decimicrodegrees (10⁻⁷).
    pub fn decimicro_lat(&self) -> i32 {
        (self.nano_lat() / 100) as i32
    }

    /// Returns the longitude coordinate in degrees.
    pub fn lon(&self) -> f64 {
        1e-9 * self.nano_lon() as f64
    }

    /// Returns the longitude in nanodegrees (10⁻⁹).
    pub fn nano_lon(&self) -> i64 {
        self.block.lon_offset() + i64::from(self.block.granularity()) * self.lon
    }

    /// Returns the longitude coordinate in decimicrodegrees (10⁻⁷).
    pub fn decimicro_lon(&self) -> i32 {
        (self.nano_lon() / 100) as i32
    }

    /// Returns an iterator over the tags of this node (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    ///
    /// Iteration is best effort: a tag whose key or value index is out of
    /// bounds, or whose string is not valid UTF-8, is skipped instead of
    /// ending the iteration.
    pub fn tags(&self) -> DenseTagIter<'a> {
        DenseTagIter {
            block: self.block,
            keys_vals: self.keys_vals,
            pos: 0,
        }
    }

    /// Returns an iterator over the tags of this node
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of indices (key and value) to the stringtable of the current
    /// [`PrimitiveBlock`](crate::block::PrimitiveBlock).
    pub fn raw_tags(&self) -> DenseRawTagIter<'a> {
        DenseRawTagIter {
            keys_vals: self.keys_vals,
            pos: 0,
        }
    }
}

/// An iterator over dense nodes. It decodes the delta encoded values.
#[derive(Clone, Debug)]
pub struct DenseNodeIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    ids: PackedVarints<'a>,
    lats: PackedVarints<'a>,
    lons: PackedVarints<'a>,
    keys_vals: &'a [u8],
    keys_vals_pos: usize,
    info: Option<DenseNodeInfoIter<'a>>,
}

impl<'a> DenseNodeIter<'a> {
    /// Creates an iterator over the `DenseNodes` message in `dense`.
    pub(crate) fn from_wire(
        block: &'a osmformat::PrimitiveBlock,
        dense: &'a [u8],
    ) -> DenseNodeIter<'a> {
        let mut ids: &[u8] = &[];
        let mut lats: &[u8] = &[];
        let mut lons: &[u8] = &[];
        let mut keys_vals: &[u8] = &[];
        let mut info = None;

        // Best effort, like the rest of the block layer: a malformed field
        // ends the scan rather than failing the whole iteration.
        let mut fields = FieldIter::new(dense);
        while let Ok(Some((field, value))) = fields.next_field() {
            match (field, value) {
                (1, WireValue::Bytes(value)) => ids = value,
                (8, WireValue::Bytes(value)) => lats = value,
                (9, WireValue::Bytes(value)) => lons = value,
                (10, WireValue::Bytes(value)) => keys_vals = value,
                (5, WireValue::Bytes(value)) if info.is_none() => {
                    info = Some(DenseNodeInfoIter::from_wire(block, value));
                }
                _ => {}
            }
        }

        DenseNodeIter {
            block,
            ids: PackedVarints::delta(ids),
            lats: PackedVarints::delta(lats),
            lons: PackedVarints::delta(lons),
            keys_vals,
            keys_vals_pos: 0,
            info,
        }
    }

    /// Creates an iterator that yields nothing.
    pub(crate) fn empty(block: &'a osmformat::PrimitiveBlock) -> DenseNodeIter<'a> {
        DenseNodeIter {
            block,
            ids: PackedVarints::delta(&[]),
            lats: PackedVarints::delta(&[]),
            lons: PackedVarints::delta(&[]),
            keys_vals: &[],
            keys_vals_pos: 0,
            info: None,
        }
    }
}

impl<'a> Iterator for DenseNodeIter<'a> {
    type Item = DenseNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let id = self.ids.next_value()?;
        let lat = self.lats.next_value()?;
        let lon = self.lons.next_value()?;

        let (keys_vals, next_pos) = scan_tags(self.keys_vals, self.keys_vals_pos);
        self.keys_vals_pos = next_pos;

        Some(DenseNode {
            block: self.block,
            id,
            lat,
            lon,
            keys_vals,
            info: self.info.as_mut().and_then(|iter| iter.next()),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Each value takes at least one byte, so the remaining bytes bound the
        // number of remaining nodes. Malformed entries may end it earlier.
        let upper = self
            .ids
            .upper_bound()
            .min(self.lats.upper_bound())
            .min(self.lons.upper_bound());
        (0, Some(upper))
    }
}

/// Finds the packed tag pairs that belong to one dense node.
///
/// Returns the byte range of the pairs and the position after them (past the
/// zero delimiter, if there is one).
#[inline]
fn scan_tags(buf: &[u8], start: usize) -> (&[u8], usize) {
    let mut pos = start;
    let mut end = start;
    while pos < buf.len() {
        let key = match read_varint_opt(buf, &mut pos) {
            Some(key) => key,
            None => break,
        };
        if key == 0 {
            break;
        }
        if read_varint_opt(buf, &mut pos).is_none() {
            break;
        }
        end = pos;
    }
    (&buf[start..end], pos)
}

/// Optional metadata with non-geographic information about a dense node
#[derive(Clone, Debug)]
pub struct DenseNodeInfo<'a> {
    block: &'a osmformat::PrimitiveBlock,
    /// The version of this element.
    version: i32,
    /// Timestamp
    timestamp: i64,
    /// The changeset id.
    changeset: i64,
    /// The user id.
    uid: i32,
    /// String IDs for usernames.
    user_sid: i32,
    /// Is the element visible (true) or was it deleted (false).
    visible: bool,
}

impl<'a> DenseNodeInfo<'a> {
    /// Returns the version of this element.
    pub fn version(&self) -> i32 {
        self.version
    }

    /// Returns the changeset id.
    pub fn changeset(&self) -> i64 {
        self.changeset
    }

    /// Returns the user id.
    pub fn uid(&self) -> i32 {
        self.uid
    }

    /// Returns the user name.
    pub fn user(&self) -> Result<&'a str> {
        str_from_stringtable(self.block, self.user_sid as usize)
    }

    /// Returns the time stamp in milliseconds since the epoch.
    pub fn milli_timestamp(&self) -> i64 {
        self.timestamp * i64::from(self.block.date_granularity())
    }

    /// Returns the visibility status of an element. This is only relevant if the PBF file contains
    /// historical information.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Returns true if the element was deleted.
    /// This is a convenience function that just returns the inverse of `DenseNodeInfo::visible`.
    pub fn deleted(&self) -> bool {
        !self.visible
    }
}

/// An iterator over dense nodes info. It decodes the delta encoded values.
#[derive(Clone, Debug)]
pub struct DenseNodeInfoIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    versions: PackedVarints<'a>,
    timestamps: PackedVarints<'a>,
    changesets: PackedVarints<'a>,
    uids: PackedVarints<'a>,
    user_sids: PackedVarints<'a>,
    visible: PackedVarints<'a>,
}

impl<'a> DenseNodeInfoIter<'a> {
    /// Creates an iterator over a `DenseInfo` message.
    pub(crate) fn from_wire(
        block: &'a osmformat::PrimitiveBlock,
        info: &'a [u8],
    ) -> DenseNodeInfoIter<'a> {
        let mut versions: &[u8] = &[];
        let mut timestamps: &[u8] = &[];
        let mut changesets: &[u8] = &[];
        let mut uids: &[u8] = &[];
        let mut user_sids: &[u8] = &[];
        let mut visible: &[u8] = &[];

        let mut fields = FieldIter::new(info);
        while let Ok(Some((field, value))) = fields.next_field() {
            if let WireValue::Bytes(value) = value {
                match field {
                    1 => versions = value,
                    2 => timestamps = value,
                    3 => changesets = value,
                    4 => uids = value,
                    5 => user_sids = value,
                    6 => visible = value,
                    _ => {}
                }
            }
        }

        DenseNodeInfoIter {
            block,
            versions: PackedVarints::plain(versions),
            timestamps: PackedVarints::delta(timestamps),
            changesets: PackedVarints::delta(changesets),
            uids: PackedVarints::delta(uids),
            user_sids: PackedVarints::delta(user_sids),
            visible: PackedVarints::plain(visible),
        }
    }
}

impl<'a> Iterator for DenseNodeInfoIter<'a> {
    type Item = DenseNodeInfo<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let version = self.versions.next_value()? as i32;
        let timestamp = self.timestamps.next_value()?;
        let changeset = self.changesets.next_value()?;
        let uid = self.uids.next_value()? as i32;
        let user_sid = self.user_sids.next_value()? as i32;
        let visible = self.visible.next_value().unwrap_or(1) != 0;

        Some(DenseNodeInfo {
            block: self.block,
            version,
            timestamp,
            changeset,
            uid,
            user_sid,
            visible,
        })
    }
}

/// An iterator over the tags in a dense node.
///
/// Iteration is best effort: a tag whose key or value index is out of bounds,
/// or whose string is not valid UTF-8, is skipped instead of ending the
/// iteration. Use the node's `raw_tags` method if such data must be handled
/// exactly.
#[derive(Clone, Debug)]
pub struct DenseTagIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    keys_vals: &'a [u8],
    pos: usize,
}

impl<'a> DenseTagIter<'a> {
    /// Reads the next raw index, or `None` at the end of the array.
    #[inline]
    fn next_index(&mut self) -> Option<usize> {
        let value = read_varint_opt(self.keys_vals, &mut self.pos)?;
        Some(value as usize)
    }
}

impl<'a> Iterator for DenseTagIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let key_index = self.next_index()?;
            let val_index = self.next_index()?;
            if let Some(tag) =
                get_stringtable_key_value(self.block, Some(key_index), Some(val_index))
            {
                return Some(tag);
            }
            // Skip the malformed tag instead of ending the iteration early.
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Malformed entries are skipped, so only the upper bound is known.
        (0, Some((self.keys_vals.len() - self.pos) / 2))
    }
}

/// An iterator over the tags of a node. It returns a pair of indices (key and value) to the
/// stringtable of the current [`PrimitiveBlock`](crate::block::PrimitiveBlock).
#[derive(Clone, Debug)]
pub struct DenseRawTagIter<'a> {
    keys_vals: &'a [u8],
    pos: usize,
}

impl<'a> DenseRawTagIter<'a> {
    /// Reads the next raw index, or `None` at the end of the array.
    #[inline]
    fn next_index(&mut self) -> Option<i32> {
        let value = read_varint_opt(self.keys_vals, &mut self.pos)?;
        Some(value as i32)
    }
}

impl Iterator for DenseRawTagIter<'_> {
    type Item = (i32, i32);

    fn next(&mut self) -> Option<Self::Item> {
        let key_index = self.next_index()?;
        let val_index = self.next_index()?;
        Some((key_index, val_index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Each index takes at least one byte, so this bounds the pairs.
        (0, Some((self.keys_vals.len() - self.pos) / 2))
    }
}
