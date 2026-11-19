//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use crate::dense::DenseNodeIter;
use crate::elements::{Element, Node, Relation, Way};
use crate::error::{ErrorKind, Result, new_error};
use crate::wire::{FieldIter, WireValue, read_varint};
use std::ops::Range;

/// A `HeaderBlock`. It contains metadata about following [`PrimitiveBlock`]s.
#[derive(Clone, Debug)]
pub struct HeaderBlock {
    bbox: Option<HeaderBBox>,
    required_features: Vec<String>,
    optional_features: Vec<String>,
    writingprogram: Option<String>,
    source: Option<String>,
    osmosis_replication_timestamp: Option<i64>,
    osmosis_replication_sequence_number: Option<i64>,
    osmosis_replication_base_url: Option<String>,
}

impl HeaderBlock {
    /// Parses a serialized `HeaderBlock`.
    pub(crate) fn parse(data: &[u8]) -> Result<HeaderBlock> {
        let mut header = HeaderBlock {
            bbox: None,
            required_features: Vec::new(),
            optional_features: Vec::new(),
            writingprogram: None,
            source: None,
            osmosis_replication_timestamp: None,
            osmosis_replication_sequence_number: None,
            osmosis_replication_base_url: None,
        };

        let mut fields = FieldIter::new(data);
        while let Some((field, value)) = fields.next_field()? {
            match (field, value) {
                (1, WireValue::Bytes(bytes)) => header.bbox = Some(HeaderBBox::parse(bytes)?),
                (4, WireValue::Bytes(bytes)) => {
                    header.required_features.push(utf8(bytes)?);
                }
                (5, WireValue::Bytes(bytes)) => {
                    header.optional_features.push(utf8(bytes)?);
                }
                (16, WireValue::Bytes(bytes)) => header.writingprogram = Some(utf8(bytes)?),
                (17, WireValue::Bytes(bytes)) => header.source = Some(utf8(bytes)?),
                (32, WireValue::Varint(value)) => {
                    header.osmosis_replication_timestamp = Some(value as i64)
                }
                (33, WireValue::Varint(value)) => {
                    header.osmosis_replication_sequence_number = Some(value as i64)
                }
                (34, WireValue::Bytes(bytes)) => {
                    header.osmosis_replication_base_url = Some(utf8(bytes)?)
                }
                _ => {}
            }
        }

        Ok(header)
    }

    /// Returns the (optional) bounding box of the included features.
    pub fn bbox(&self) -> Option<HeaderBBox> {
        self.bbox.clone()
    }

    /// Returns a list of required features that a parser needs to implement to parse the following
    /// [`PrimitiveBlock`]s.
    pub fn required_features(&self) -> &[String] {
        self.required_features.as_slice()
    }

    /// Returns a list of optional features that a parser can choose to ignore.
    pub fn optional_features(&self) -> &[String] {
        self.optional_features.as_slice()
    }

    /// Returns the name of the program that generated the file or `None` if unset.
    pub fn writing_program(&self) -> Option<&str> {
        self.writingprogram.as_deref()
    }

    /// Returns the source of the `bbox` field or `None` if unset.
    pub fn source(&self) -> Option<&str> {
        self.source.as_deref()
    }

    /// Returns the replication timestamp of the file, or `None` if unset.
    /// The timestamp is expressed in seconds since the UNIX epoch.
    pub fn osmosis_replication_timestamp(&self) -> Option<i64> {
        self.osmosis_replication_timestamp
    }

    /// Returns the replication sequence number of the file, or `None` if unset.
    pub fn osmosis_replication_sequence_number(&self) -> Option<i64> {
        self.osmosis_replication_sequence_number
    }

    /// Returns the replication base URL of the file, or `None` if unset.
    pub fn osmosis_replication_base_url(&self) -> Option<&str> {
        self.osmosis_replication_base_url.as_deref()
    }
}

/// Controls its visibility of an element.
#[derive(Clone, Debug)]
pub struct HeaderBBox {
    /// left coordinate in degrees (minimum longitude, west edge)
    pub left: f64,
    /// right coordinate in degrees (maximum longitude, east edge)
    pub right: f64,
    /// top coordinate in degrees (maximum latitude, north edge)
    pub top: f64,
    /// bottom coordinate in degrees (minimum latitude, south edge)
    pub bottom: f64,
}

impl HeaderBBox {
    /// Parses a serialized `HeaderBBox` message.
    fn parse(data: &[u8]) -> Result<HeaderBBox> {
        let mut left = None;
        let mut right = None;
        let mut top = None;
        let mut bottom = None;

        let mut fields = FieldIter::new(data);
        while let Some((field, value)) = fields.next_field()? {
            if let WireValue::Varint(value) = value {
                let value = crate::wire::zigzag(value);
                match field {
                    1 => left = Some(value),
                    2 => right = Some(value),
                    3 => top = Some(value),
                    4 => bottom = Some(value),
                    _ => {}
                }
            }
        }

        let missing = || {
            new_error(ErrorKind::MalformedBlock {
                reason: "header bbox is missing a required field",
            })
        };

        Ok(HeaderBBox {
            left: (left.ok_or_else(missing)? as f64) * 1.0_e-9,
            right: (right.ok_or_else(missing)? as f64) * 1.0_e-9,
            top: (top.ok_or_else(missing)? as f64) * 1.0_e-9,
            bottom: (bottom.ok_or_else(missing)? as f64) * 1.0_e-9,
        })
    }
}

/// A `PrimitiveBlock`. It contains a sequence of groups.
#[derive(Clone, Debug)]
pub struct PrimitiveBlock {
    /// The whole decompressed block. Groups and elements are slices into it.
    data: Vec<u8>,
    /// The string table, decoded eagerly: it is small and indexed by every tag.
    strings: Vec<Vec<u8>>,
    granularity: i32,
    lat_offset: i64,
    lon_offset: i64,
    date_granularity: i32,
    groups: Vec<GroupInfo>,
}

/// Where a group lives in the block, and how many elements it holds.
#[derive(Clone, Debug)]
struct GroupInfo {
    range: Range<usize>,
    nodes: usize,
    ways: usize,
    relations: usize,
    has_dense: bool,
}

impl PrimitiveBlock {
    /// Parses a serialized `PrimitiveBlock`.
    ///
    /// The string table and the block-wide parameters are decoded eagerly (they
    /// are small); elements stay in their wire format and are decoded one at a
    /// time.
    pub(crate) fn parse(data: Vec<u8>) -> Result<PrimitiveBlock> {
        let mut strings = Vec::new();
        let mut granularity = 100;
        let mut lat_offset = 0;
        let mut lon_offset = 0;
        let mut date_granularity = 1000;
        let mut groups = Vec::new();
        let mut has_stringtable = false;

        let mut pos = 0;
        while pos < data.len() {
            let tag = read_varint(&data, &mut pos)?;
            let field = tag >> 3;
            match tag & 7 {
                0 => {
                    let value = read_varint(&data, &mut pos)?;
                    match field {
                        17 => granularity = value as i32,
                        18 => date_granularity = value as i32,
                        19 => lat_offset = value as i64,
                        20 => lon_offset = value as i64,
                        _ => {}
                    }
                }
                2 => {
                    let len = read_varint(&data, &mut pos)? as usize;
                    let start = pos;
                    let end = start
                        .checked_add(len)
                        .ok_or_else(|| malformed_block("field length overflows"))?;
                    if end > data.len() {
                        return Err(malformed_block("truncated field"));
                    }
                    pos = end;
                    match field {
                        1 => {
                            strings = parse_string_table(&data[start..end])?;
                            has_stringtable = true;
                        }
                        2 => groups.push(parse_group_info(start..end, &data[start..end])?),
                        _ => {}
                    }
                }
                1 => pos += 8,
                5 => pos += 4,
                _ => return Err(malformed_block("unexpected wire type")),
            }
        }

        // `stringtable` is a proto2 required field; a payload without it is not
        // a PrimitiveBlock (a header blob, for example).
        if !has_stringtable {
            return Err(malformed_block("missing required stringtable"));
        }

        Ok(PrimitiveBlock {
            data,
            strings,
            granularity,
            lat_offset,
            lon_offset,
            date_granularity,
            groups,
        })
    }

    /// Returns the granularity of coordinates in nanodegrees.
    pub(crate) fn granularity(&self) -> i32 {
        self.granularity
    }

    /// Returns the latitude offset in nanodegrees.
    pub(crate) fn lat_offset(&self) -> i64 {
        self.lat_offset
    }

    /// Returns the longitude offset in nanodegrees.
    pub(crate) fn lon_offset(&self) -> i64 {
        self.lon_offset
    }

    /// Returns the granularity of timestamps in milliseconds.
    pub(crate) fn date_granularity(&self) -> i32 {
        self.date_granularity
    }

    /// Returns an iterator over the elements in this `PrimitiveBlock`.
    pub fn elements(&self) -> BlockElementsIter<'_> {
        BlockElementsIter::new(self)
    }

    /// Returns an iterator over the groups in this `PrimitiveBlock`.
    pub fn groups(&self) -> GroupIter<'_> {
        GroupIter::new(self)
    }

    /// Calls the given closure on each element.
    pub fn for_each_element<F>(&self, mut f: F)
    where
        F: for<'a> FnMut(Element<'a>),
    {
        for group in self.groups() {
            for node in group.nodes() {
                f(Element::Node(node))
            }
            for dnode in group.dense_nodes() {
                f(Element::DenseNode(dnode))
            }
            for way in group.ways() {
                f(Element::Way(way));
            }
            for relation in group.relations() {
                f(Element::Relation(relation));
            }
        }
    }

    /// Returns the raw stringtable. Elements in a `PrimitiveBlock` do not store strings
    /// themselves; instead, they just store indices to the stringtable. By convention, the
    /// contained strings are UTF-8 encoded but it is not safe to assume that (use
    /// `std::str::from_utf8`).
    pub fn raw_stringtable(&self) -> &[Vec<u8>] {
        self.strings.as_slice()
    }
}

/// Decodes a `StringTable` message into its list of strings.
fn parse_string_table(data: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut strings = Vec::new();
    let mut fields = FieldIter::new(data);
    while let Some((field, value)) = fields.next_field()? {
        match (field, value) {
            (1, WireValue::Bytes(string)) => strings.push(string.to_vec()),
            (1, WireValue::Varint(_)) => return Err(malformed_block("unexpected wire type")),
            _ => {}
        }
    }
    Ok(strings)
}

/// Counts the elements of one group and checks its dense arrays.
fn parse_group_info(range: Range<usize>, data: &[u8]) -> Result<GroupInfo> {
    let mut nodes = 0;
    let mut ways = 0;
    let mut relations = 0;
    let mut has_dense = false;

    let mut fields = FieldIter::new(data);
    while let Some((field, value)) = fields.next_field()? {
        match (field, value) {
            (1, WireValue::Bytes(_)) => nodes += 1,
            (2, WireValue::Bytes(bytes)) => {
                has_dense = true;
                validate_dense(bytes)?;
            }
            (3, WireValue::Bytes(_)) => ways += 1,
            (4, WireValue::Bytes(_)) => relations += 1,
            _ => {}
        }
    }

    Ok(GroupInfo {
        range,
        nodes,
        ways,
        relations,
        has_dense,
    })
}

/// Rejects dense node arrays that are not packed.
///
/// The schema declares `[packed = true]`, so every writer packs these arrays.
/// A file that sends them unpacked would otherwise decode to zero nodes
/// without any error, which is exactly the kind of silent loss this crate
/// tries to avoid.
fn validate_dense(data: &[u8]) -> Result<()> {
    let mut fields = FieldIter::new(data);
    while let Some((field, value)) = fields.next_field()? {
        if matches!(field, 1 | 8 | 9 | 10) && matches!(value, WireValue::Varint(_)) {
            return Err(malformed_block("dense arrays must be packed"));
        }
    }
    Ok(())
}

/// Builds an error for a block that does not follow the wire format.
fn malformed_block(reason: &'static str) -> crate::error::Error {
    new_error(ErrorKind::MalformedBlock { reason })
}

/// Decodes a length-delimited string field as UTF-8.
fn utf8(bytes: &[u8]) -> Result<String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(text.to_owned()),
        Err(_) => Err(malformed_block("string field is not valid UTF-8")),
    }
}

/// A `PrimitiveGroup` contains a sequence of elements of one type.
#[derive(Clone, Debug)]
pub struct PrimitiveGroup<'a> {
    block: &'a PrimitiveBlock,
    info: &'a GroupInfo,
}

impl<'a> PrimitiveGroup<'a> {
    /// Creates a group view.
    fn new(block: &'a PrimitiveBlock, info: &'a GroupInfo) -> PrimitiveGroup<'a> {
        PrimitiveGroup { block, info }
    }

    /// The serialized group this view refers to.
    fn bytes(&self) -> &'a [u8] {
        &self.block.data[self.info.range.clone()]
    }

    /// Returns an iterator over the nodes in this group.
    pub fn nodes(&self) -> GroupNodeIter<'a> {
        GroupNodeIter::new(self.block, self.bytes(), self.info.nodes)
    }

    /// Returns an iterator over the dense nodes in this group.
    pub fn dense_nodes(&self) -> DenseNodeIter<'a> {
        if !self.info.has_dense {
            return DenseNodeIter::empty(self.block);
        }
        let mut fields = FieldIter::new(self.bytes());
        while let Ok(Some((field, value))) = fields.next_field() {
            if let (2, WireValue::Bytes(bytes)) = (field, value) {
                return DenseNodeIter::from_wire(self.block, bytes);
            }
        }
        DenseNodeIter::empty(self.block)
    }

    /// Returns an iterator over the ways in this group.
    pub fn ways(&self) -> GroupWayIter<'a> {
        GroupWayIter::new(self.block, self.bytes(), self.info.ways)
    }

    /// Returns an iterator over the relations in this group.
    pub fn relations(&self) -> GroupRelationIter<'a> {
        GroupRelationIter::new(self.block, self.bytes(), self.info.relations)
    }
}

/// An iterator over the elements in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct BlockElementsIter<'a> {
    block: &'a PrimitiveBlock,
    state: ElementsIterState,
    groups: std::slice::Iter<'a, GroupInfo>,
    dense_nodes: DenseNodeIter<'a>,
    nodes: GroupNodeIter<'a>,
    ways: GroupWayIter<'a>,
    relations: GroupRelationIter<'a>,
}

#[derive(Copy, Clone, Debug)]
enum ElementsIterState {
    Group,
    DenseNode,
    Node,
    Way,
    Relation,
}

impl<'a> BlockElementsIter<'a> {
    fn new(block: &'a PrimitiveBlock) -> BlockElementsIter<'a> {
        BlockElementsIter {
            block,
            state: ElementsIterState::Group,
            groups: block.groups.iter(),
            dense_nodes: DenseNodeIter::empty(block),
            nodes: GroupNodeIter::empty(block),
            ways: GroupWayIter::empty(block),
            relations: GroupRelationIter::empty(block),
        }
    }

    /// Performs an internal iteration step. Returns [`None`] until there is a value for the iterator to
    /// return. Returns [`Some(None)`] to end the iteration.
    #[inline]
    #[allow(clippy::option_option)]
    fn step(&mut self) -> Option<Option<Element<'a>>> {
        match self.state {
            ElementsIterState::Group => match self.groups.next() {
                Some(info) => {
                    let group = PrimitiveGroup::new(self.block, info);
                    self.state = ElementsIterState::DenseNode;
                    self.dense_nodes = group.dense_nodes();
                    self.nodes = group.nodes();
                    self.ways = group.ways();
                    self.relations = group.relations();
                    None
                }
                None => Some(None),
            },
            ElementsIterState::DenseNode => match self.dense_nodes.next() {
                Some(dense_node) => Some(Some(Element::DenseNode(dense_node))),
                None => {
                    self.state = ElementsIterState::Node;
                    None
                }
            },
            ElementsIterState::Node => match self.nodes.next() {
                Some(node) => Some(Some(Element::Node(node))),
                None => {
                    self.state = ElementsIterState::Way;
                    None
                }
            },
            ElementsIterState::Way => match self.ways.next() {
                Some(way) => Some(Some(Element::Way(way))),
                None => {
                    self.state = ElementsIterState::Relation;
                    None
                }
            },
            ElementsIterState::Relation => match self.relations.next() {
                Some(rel) => Some(Some(Element::Relation(rel))),
                None => {
                    self.state = ElementsIterState::Group;
                    None
                }
            },
        }
    }
}

impl<'a> Iterator for BlockElementsIter<'a> {
    type Item = Element<'a>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(element) = self.step() {
                return element;
            }
        }
    }
}

/// An iterator over the groups in a [`PrimitiveBlock`].
#[derive(Clone, Debug)]
pub struct GroupIter<'a> {
    block: &'a PrimitiveBlock,
    groups: std::slice::Iter<'a, GroupInfo>,
}

impl<'a> GroupIter<'a> {
    fn new(block: &'a PrimitiveBlock) -> GroupIter<'a> {
        GroupIter {
            block,
            groups: block.groups.iter(),
        }
    }
}

impl<'a> Iterator for GroupIter<'a> {
    type Item = PrimitiveGroup<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.groups.next() {
            Some(info) => Some(PrimitiveGroup::new(self.block, info)),
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.groups.size_hint()
    }
}

impl ExactSizeIterator for GroupIter<'_> {}

/// An iterator over the nodes in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupNodeIter<'a> {
    block: &'a PrimitiveBlock,
    fields: FieldIter<'a>,
    remaining: usize,
}

impl<'a> GroupNodeIter<'a> {
    fn new(block: &'a PrimitiveBlock, data: &'a [u8], remaining: usize) -> GroupNodeIter<'a> {
        GroupNodeIter {
            block,
            fields: FieldIter::new(data),
            remaining,
        }
    }

    fn empty(block: &'a PrimitiveBlock) -> GroupNodeIter<'a> {
        GroupNodeIter {
            block,
            fields: FieldIter::new(&[]),
            remaining: 0,
        }
    }
}

impl<'a> Iterator for GroupNodeIter<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.fields.next_field() {
                Ok(Some((1, WireValue::Bytes(bytes)))) => {
                    self.remaining = self.remaining.saturating_sub(1);
                    return Some(Node::from_wire(self.block, bytes));
                }
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for GroupNodeIter<'_> {}

/// An iterator over the ways in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupWayIter<'a> {
    block: &'a PrimitiveBlock,
    fields: FieldIter<'a>,
    remaining: usize,
}

impl<'a> GroupWayIter<'a> {
    fn new(block: &'a PrimitiveBlock, data: &'a [u8], remaining: usize) -> GroupWayIter<'a> {
        GroupWayIter {
            block,
            fields: FieldIter::new(data),
            remaining,
        }
    }

    fn empty(block: &'a PrimitiveBlock) -> GroupWayIter<'a> {
        GroupWayIter {
            block,
            fields: FieldIter::new(&[]),
            remaining: 0,
        }
    }
}

impl<'a> Iterator for GroupWayIter<'a> {
    type Item = Way<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.fields.next_field() {
                Ok(Some((3, WireValue::Bytes(bytes)))) => {
                    self.remaining = self.remaining.saturating_sub(1);
                    return Some(Way::from_wire(self.block, bytes));
                }
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for GroupWayIter<'_> {}

/// An iterator over the relations in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupRelationIter<'a> {
    block: &'a PrimitiveBlock,
    fields: FieldIter<'a>,
    remaining: usize,
}

impl<'a> GroupRelationIter<'a> {
    fn new(block: &'a PrimitiveBlock, data: &'a [u8], remaining: usize) -> GroupRelationIter<'a> {
        GroupRelationIter {
            block,
            fields: FieldIter::new(data),
            remaining,
        }
    }

    fn empty(block: &'a PrimitiveBlock) -> GroupRelationIter<'a> {
        GroupRelationIter {
            block,
            fields: FieldIter::new(&[]),
            remaining: 0,
        }
    }
}

impl<'a> Iterator for GroupRelationIter<'a> {
    type Item = Relation<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.fields.next_field() {
                Ok(Some((4, WireValue::Bytes(bytes)))) => {
                    self.remaining = self.remaining.saturating_sub(1);
                    return Some(Relation::from_wire(self.block, bytes));
                }
                Ok(Some(_)) => {}
                _ => return None,
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for GroupRelationIter<'_> {}

/// Looks up a string in the string table of a block.
pub(crate) fn str_from_stringtable(block: &PrimitiveBlock, index: usize) -> Result<&str> {
    if let Some(vec) = block.strings.get(index) {
        std::str::from_utf8(vec)
            .map_err(|e| new_error(ErrorKind::StringtableUtf8 { err: e, index }))
    } else {
        Err(new_error(ErrorKind::StringtableIndexOutOfBounds { index }))
    }
}

/// Construct a key-value tuple from key/value indexes, using the stringtable from a block.
pub(crate) fn get_stringtable_key_value(
    block: &PrimitiveBlock,
    key_index: Option<usize>,
    value_index: Option<usize>,
) -> Option<(&str, &str)> {
    match (key_index, value_index) {
        (Some(key_index), Some(val_index)) => {
            let k_res = str_from_stringtable(block, key_index);
            let v_res = str_from_stringtable(block, val_index);
            if let (Ok(k), Ok(v)) = (k_res, v_res) {
                Some((k, v))
            } else {
                None
            }
        }
        _ => None,
    }
}
