//! `HeaderBlock`, `PrimitiveBlock` and `PrimitiveGroup`s

use crate::dense::DenseNodeIter;
use crate::elements::{Element, Node, Relation, Way};
use crate::error::{ErrorKind, Result, new_error, new_protobuf_error};
use crate::proto::osmformat;
use crate::wire::{FieldIter, WireValue};
use protobuf::Message;
use std;

/// A `HeaderBlock`. It contains metadata about following [`PrimitiveBlock`]s.
#[derive(Clone, Debug)]
pub struct HeaderBlock {
    header: osmformat::HeaderBlock,
}

impl HeaderBlock {
    pub(crate) fn new(header: osmformat::HeaderBlock) -> HeaderBlock {
        HeaderBlock { header }
    }

    /// Returns the (optional) bounding box of the included features.
    pub fn bbox(&self) -> Option<HeaderBBox> {
        self.header.bbox.as_ref().map(|bbox| HeaderBBox {
            left: (bbox.left() as f64) * 1.0_e-9,
            right: (bbox.right() as f64) * 1.0_e-9,
            top: (bbox.top() as f64) * 1.0_e-9,
            bottom: (bbox.bottom() as f64) * 1.0_e-9,
        })
    }

    /// Returns a list of required features that a parser needs to implement to parse the following
    /// [`PrimitiveBlock`]s.
    pub fn required_features(&self) -> &[String] {
        self.header.required_features.as_slice()
    }

    /// Returns a list of optional features that a parser can choose to ignore.
    pub fn optional_features(&self) -> &[String] {
        self.header.optional_features.as_slice()
    }

    /// Returns the name of the program that generated the file or `None` if unset.
    pub fn writing_program(&self) -> Option<&str> {
        if self.header.has_writingprogram() {
            Some(self.header.writingprogram())
        } else {
            None
        }
    }

    /// Returns the source of the `bbox` field or `None` if unset.
    pub fn source(&self) -> Option<&str> {
        if self.header.has_source() {
            Some(self.header.source())
        } else {
            None
        }
    }

    /// Returns the replication timestamp of the file, or `None` if unset.
    /// The timestamp is expressed in seconds since the UNIX epoch.
    pub fn osmosis_replication_timestamp(&self) -> Option<i64> {
        if self.header.has_osmosis_replication_timestamp() {
            Some(self.header.osmosis_replication_timestamp())
        } else {
            None
        }
    }

    /// Returns the replication sequence number of the file, or `None` if unset.
    pub fn osmosis_replication_sequence_number(&self) -> Option<i64> {
        if self.header.has_osmosis_replication_sequence_number() {
            Some(self.header.osmosis_replication_sequence_number())
        } else {
            None
        }
    }

    /// Returns the replication base URL of the file, or `None` if unset.
    pub fn osmosis_replication_base_url(&self) -> Option<&str> {
        if self.header.has_osmosis_replication_base_url() {
            Some(self.header.osmosis_replication_base_url())
        } else {
            None
        }
    }
}

/// A bounding box that is usually included in a [`HeaderBlock`].
/// The maximum precision of the coordinates is one nanodegree (10⁻⁹).
///
/// The field names follow the OSM API `bbox` convention, which is used by the
/// writers of the format: `left`/`right` are the west/east edges and
/// `top`/`bottom` are the north/south edges. So `top` holds the *maximum*
/// latitude and `bottom` the *minimum* latitude, which is the opposite of what
/// the words suggest when read as a rectangle on screen.
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

/// A `PrimitiveBlock`. It contains a sequence of groups.
#[derive(Clone, Debug)]
pub struct PrimitiveBlock {
    /// Block-wide fields: string table, granularity, coordinate offsets. Its
    /// `primitivegroup` field is always empty; groups are stored separately so
    /// that dense nodes can be decoded straight from the wire.
    block: osmformat::PrimitiveBlock,
    groups: Vec<GroupData>,
}

/// How one group of a [`PrimitiveBlock`] is stored.
#[derive(Clone, Debug)]
enum GroupData {
    /// A group whose dense nodes are decoded on demand from the wire format.
    /// `other` is only set for a group that mixes dense nodes with other
    /// elements, which the format does not produce but a malformed file might.
    Dense {
        dense: Vec<u8>,
        other: Option<osmformat::PrimitiveGroup>,
    },
    /// A group decoded with protobuf.
    Proto(osmformat::PrimitiveGroup),
}

impl PrimitiveBlock {
    /// Parses a serialized `PrimitiveBlock`.
    ///
    /// The string table and the block-wide parameters are decoded eagerly (they
    /// are small); dense node arrays stay in their wire format and are decoded
    /// element by element.
    pub(crate) fn parse(data: &[u8]) -> Result<PrimitiveBlock> {
        let mut strings = Vec::new();
        let mut granularity = 100;
        let mut lat_offset = 0;
        let mut lon_offset = 0;
        let mut date_granularity = 1000;
        let mut groups = Vec::new();
        let mut has_stringtable = false;

        let mut fields = FieldIter::new(data);
        while let Some((field, value)) = fields.next_field()? {
            match (field, value) {
                (1, WireValue::Bytes(bytes)) => {
                    strings = parse_string_table(bytes)?;
                    has_stringtable = true;
                }
                (2, WireValue::Bytes(bytes)) => groups.push(parse_group(bytes)?),
                (17, WireValue::Varint(value)) => granularity = value as i32,
                (18, WireValue::Varint(value)) => date_granularity = value as i32,
                (19, WireValue::Varint(value)) => lat_offset = value as i64,
                (20, WireValue::Varint(value)) => lon_offset = value as i64,
                // A known field with the wrong wire type means this is not a
                // `PrimitiveBlock` at all (for example a header blob fed to
                // `to_primitiveblock`), so fail instead of misreading it.
                (1 | 2, WireValue::Varint(_)) | (17..=20, WireValue::Bytes(_)) => {
                    return Err(malformed_block("unexpected wire type"));
                }
                // Unknown fields are skipped on purpose: the format is allowed
                // to grow, and a future field must not make old readers fail.
                _ => {}
            }
        }

        // `stringtable` is a proto2 required field; a payload without it is not
        // a PrimitiveBlock (a header blob, for example).
        if !has_stringtable {
            return Err(malformed_block("missing required stringtable"));
        }

        let mut block = osmformat::PrimitiveBlock::new();
        block.stringtable = protobuf::MessageField::some(osmformat::StringTable {
            s: strings,
            ..Default::default()
        });
        block.granularity = Some(granularity);
        block.lat_offset = Some(lat_offset);
        block.lon_offset = Some(lon_offset);
        block.date_granularity = Some(date_granularity);

        Ok(PrimitiveBlock { block, groups })
    }

    /// Returns an iterator over the elements in this `PrimitiveBlock`.
    pub fn elements(&self) -> BlockElementsIter<'_> {
        BlockElementsIter::new(&self.block, &self.groups)
    }

    /// Returns an iterator over the groups in this `PrimitiveBlock`.
    pub fn groups(&self) -> GroupIter<'_> {
        GroupIter::new(&self.block, &self.groups)
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
        self.block.stringtable.s.as_slice()
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

/// Decides how a `PrimitiveGroup` should be stored.
fn parse_group(data: &[u8]) -> Result<GroupData> {
    let mut dense: Option<&[u8]> = None;
    let mut has_other = false;

    let mut fields = FieldIter::new(data);
    while let Some((field, value)) = fields.next_field()? {
        match (field, value) {
            (2, WireValue::Bytes(bytes)) => dense = Some(bytes),
            (1 | 3 | 4 | 5, _) => has_other = true,
            _ => {}
        }
    }

    match dense {
        None => Ok(GroupData::Proto(parse_primitive_group(data)?)),
        Some(dense) => {
            validate_dense(dense)?;
            let other = if has_other {
                Some(parse_primitive_group(data)?)
            } else {
                None
            };
            Ok(GroupData::Dense {
                dense: dense.to_vec(),
                other,
            })
        }
    }
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

/// Decodes a whole `PrimitiveGroup` with protobuf.
fn parse_primitive_group(data: &[u8]) -> Result<osmformat::PrimitiveGroup> {
    osmformat::PrimitiveGroup::parse_from_bytes(data)
        .map_err(|error| new_protobuf_error(error, "primitive group"))
}

/// A `PrimitiveGroup` contains a sequence of elements of one type.
#[derive(Clone, Debug)]
pub struct PrimitiveGroup<'a> {
    block: &'a osmformat::PrimitiveBlock,
    data: &'a GroupData,
}

impl<'a> PrimitiveGroup<'a> {
    fn new(block: &'a osmformat::PrimitiveBlock, data: &'a GroupData) -> PrimitiveGroup<'a> {
        PrimitiveGroup { block, data }
    }

    /// Elements of this group that were decoded with protobuf, if any.
    fn proto(&self) -> Option<&'a osmformat::PrimitiveGroup> {
        match self.data {
            GroupData::Dense { other, .. } => other.as_ref(),
            GroupData::Proto(group) => Some(group),
        }
    }

    /// The nodes of this group, or an empty slice for a dense group.
    fn node_slice(&self) -> &'a [osmformat::Node] {
        match self.proto() {
            Some(group) => group.nodes.as_slice(),
            None => &[],
        }
    }

    /// The ways of this group, or an empty slice for a dense group.
    fn way_slice(&self) -> &'a [osmformat::Way] {
        match self.proto() {
            Some(group) => group.ways.as_slice(),
            None => &[],
        }
    }

    /// The relations of this group, or an empty slice for a dense group.
    fn relation_slice(&self) -> &'a [osmformat::Relation] {
        match self.proto() {
            Some(group) => group.relations.as_slice(),
            None => &[],
        }
    }

    /// Returns an iterator over the nodes in this group.
    pub fn nodes(&self) -> GroupNodeIter<'a> {
        GroupNodeIter::new(self.block, self.node_slice())
    }

    /// Returns an iterator over the dense nodes in this group.
    pub fn dense_nodes(&self) -> DenseNodeIter<'a> {
        match self.data {
            GroupData::Dense { dense, .. } => DenseNodeIter::from_wire(self.block, dense),
            GroupData::Proto(_) => DenseNodeIter::empty(self.block),
        }
    }

    /// Returns an iterator over the ways in this group.
    pub fn ways(&self) -> GroupWayIter<'a> {
        GroupWayIter::new(self.block, self.way_slice())
    }

    /// Returns an iterator over the relations in this group.
    pub fn relations(&self) -> GroupRelationIter<'a> {
        GroupRelationIter::new(self.block, self.relation_slice())
    }
}

/// An iterator over the elements in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct BlockElementsIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    state: ElementsIterState,
    groups: std::slice::Iter<'a, GroupData>,
    dense_nodes: DenseNodeIter<'a>,
    nodes: std::slice::Iter<'a, osmformat::Node>,
    ways: std::slice::Iter<'a, osmformat::Way>,
    relations: std::slice::Iter<'a, osmformat::Relation>,
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
    fn new(block: &'a osmformat::PrimitiveBlock, groups: &'a [GroupData]) -> BlockElementsIter<'a> {
        BlockElementsIter {
            block,
            state: ElementsIterState::Group,
            groups: groups.iter(),
            dense_nodes: DenseNodeIter::empty(block),
            nodes: [].iter(),
            ways: [].iter(),
            relations: [].iter(),
        }
    }

    /// Performs an internal iteration step. Returns [`None`] until there is a value for the iterator to
    /// return. Returns [`Some(None)`] to end the iteration.
    #[inline]
    #[allow(clippy::option_option)]
    fn step(&mut self) -> Option<Option<Element<'a>>> {
        match self.state {
            ElementsIterState::Group => match self.groups.next() {
                Some(data) => {
                    let group = PrimitiveGroup::new(self.block, data);
                    self.state = ElementsIterState::DenseNode;
                    self.dense_nodes = group.dense_nodes();
                    self.nodes = group.node_slice().iter();
                    self.ways = group.way_slice().iter();
                    self.relations = group.relation_slice().iter();
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
                Some(node) => Some(Some(Element::Node(Node::new(self.block, node)))),
                None => {
                    self.state = ElementsIterState::Way;
                    None
                }
            },
            ElementsIterState::Way => match self.ways.next() {
                Some(way) => Some(Some(Element::Way(Way::new(self.block, way)))),
                None => {
                    self.state = ElementsIterState::Relation;
                    None
                }
            },
            ElementsIterState::Relation => match self.relations.next() {
                Some(rel) => Some(Some(Element::Relation(Relation::new(self.block, rel)))),
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
    block: &'a osmformat::PrimitiveBlock,
    groups: std::slice::Iter<'a, GroupData>,
}

impl<'a> GroupIter<'a> {
    fn new(block: &'a osmformat::PrimitiveBlock, groups: &'a [GroupData]) -> GroupIter<'a> {
        GroupIter {
            block,
            groups: groups.iter(),
        }
    }
}

impl<'a> Iterator for GroupIter<'a> {
    type Item = PrimitiveGroup<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.groups.next() {
            Some(data) => Some(PrimitiveGroup::new(self.block, data)),
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
    block: &'a osmformat::PrimitiveBlock,
    nodes: std::slice::Iter<'a, osmformat::Node>,
}

impl<'a> GroupNodeIter<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        nodes: &'a [osmformat::Node],
    ) -> GroupNodeIter<'a> {
        GroupNodeIter {
            block,
            nodes: nodes.iter(),
        }
    }
}

impl<'a> Iterator for GroupNodeIter<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.nodes.next() {
            Some(n) => Some(Node::new(self.block, n)),
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.nodes.size_hint()
    }
}

impl ExactSizeIterator for GroupNodeIter<'_> {}

/// An iterator over the ways in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupWayIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    ways: std::slice::Iter<'a, osmformat::Way>,
}

impl<'a> GroupWayIter<'a> {
    fn new(block: &'a osmformat::PrimitiveBlock, ways: &'a [osmformat::Way]) -> GroupWayIter<'a> {
        GroupWayIter {
            block,
            ways: ways.iter(),
        }
    }
}

impl<'a> Iterator for GroupWayIter<'a> {
    type Item = Way<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.ways.next() {
            Some(way) => Some(Way::new(self.block, way)),
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.ways.size_hint()
    }
}

impl ExactSizeIterator for GroupWayIter<'_> {}

/// An iterator over the relations in a [`PrimitiveGroup`].
#[derive(Clone, Debug)]
pub struct GroupRelationIter<'a> {
    block: &'a osmformat::PrimitiveBlock,
    rels: std::slice::Iter<'a, osmformat::Relation>,
}

impl<'a> GroupRelationIter<'a> {
    fn new(
        block: &'a osmformat::PrimitiveBlock,
        relations: &'a [osmformat::Relation],
    ) -> GroupRelationIter<'a> {
        GroupRelationIter {
            block,
            rels: relations.iter(),
        }
    }
}

impl<'a> Iterator for GroupRelationIter<'a> {
    type Item = Relation<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.rels.next() {
            Some(rel) => Some(Relation::new(self.block, rel)),
            None => None,
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rels.size_hint()
    }
}

impl ExactSizeIterator for GroupRelationIter<'_> {}

pub(crate) fn str_from_stringtable(
    block: &osmformat::PrimitiveBlock,
    index: usize,
) -> Result<&str> {
    if let Some(vec) = block.stringtable.s.get(index) {
        std::str::from_utf8(vec)
            .map_err(|e| new_error(ErrorKind::StringtableUtf8 { err: e, index }))
    } else {
        Err(new_error(ErrorKind::StringtableIndexOutOfBounds { index }))
    }
}

/// Construct a key-value tuple from key/value indexes, using the stringtable from a block.
pub(crate) fn get_stringtable_key_value(
    block: &osmformat::PrimitiveBlock,
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
