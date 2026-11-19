//! Nodes, ways and relations

use crate::block::{PrimitiveBlock, get_stringtable_key_value, str_from_stringtable};
use crate::dense::DenseNode;
use crate::error::Result;
use crate::wire::{FieldIter, PackedVarints, WireValue, zigzag};

/// An enum with the OSM core elements: nodes, ways and relations.
#[derive(Clone, Debug)]
pub enum Element<'a> {
    /// A node. Also, see [`DenseNode`](Self::DenseNode).
    Node(Node<'a>),

    /// Just like [`Node`](Self::Node), but with a different representation in memory. This distinction is
    /// usually not important but is not abstracted away to avoid copying. So, if you want to match
    /// `Node`, you also likely want to match [`DenseNode`](Self::DenseNode).
    DenseNode(DenseNode<'a>),

    /// A way.
    Way(Way<'a>),

    /// A relation.
    Relation(Relation<'a>),
}

/// An OpenStreetMap node element (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Node)).
#[derive(Clone, Debug)]
pub struct Node<'a> {
    block: &'a PrimitiveBlock,
    id: i64,
    lat: i64,
    lon: i64,
    /// Packed tag key indexes.
    keys: &'a [u8],
    /// Packed tag value indexes.
    vals: &'a [u8],
    /// The serialized `Info` message, if present.
    info: Option<&'a [u8]>,
}

impl<'a> Node<'a> {
    /// Decodes one `Node` message.
    pub(crate) fn from_wire(block: &'a PrimitiveBlock, data: &'a [u8]) -> Node<'a> {
        let mut node = Node {
            block,
            id: 0,
            lat: 0,
            lon: 0,
            keys: &[],
            vals: &[],
            info: None,
        };

        let mut fields = FieldIter::new(data);
        while let Ok(Some((field, value))) = fields.next_field() {
            match (field, value) {
                (1, WireValue::Varint(value)) => node.id = zigzag(value),
                (2, WireValue::Bytes(bytes)) => node.keys = bytes,
                (3, WireValue::Bytes(bytes)) => node.vals = bytes,
                (4, WireValue::Bytes(bytes)) => node.info = Some(bytes),
                (8, WireValue::Varint(value)) => node.lat = zigzag(value),
                (9, WireValue::Varint(value)) => node.lon = zigzag(value),
                _ => {}
            }
        }

        node
    }

    /// Returns the node id. It should be unique between nodes and might be negative to indicate
    /// that the element has not yet been uploaded to a server.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Returns an iterator over the tags of this node
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of strings (key and value).
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = ElementReader::from_path("tests/test.osm.pbf")?;
    ///
    /// reader.for_each(|element| {
    ///     if let Element::Node(node) = element {
    ///         for (key, value) in node.tags() {
    ///             println!("key: {key}, value: {value}");
    ///         }
    ///     }
    /// })?;
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            block: self.block,
            keys: PackedVarints::plain(self.keys),
            vals: PackedVarints::plain(self.vals),
        }
    }

    /// Returns additional metadata for this element.
    pub fn info(&self) -> Info<'a> {
        Info::from_wire(self.block, self.info.unwrap_or(&[]))
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

    /// Returns an iterator over the tags of this node
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of indices (key and value) to the stringtable of the current
    /// [`PrimitiveBlock`].
    pub fn raw_tags(&self) -> RawTagIter<'a> {
        RawTagIter {
            keys: PackedVarints::plain(self.keys),
            vals: PackedVarints::plain(self.vals),
        }
    }

    /// Returns the raw stringtable. Elements in a `PrimitiveBlock` do not store strings
    /// themselves; instead, they just store indices to a common stringtable. By convention, the
    /// contained strings are UTF-8 encoded but it is not safe to assume that (use
    /// `std::str::from_utf8`).
    pub fn raw_stringtable(&self) -> &[Vec<u8>] {
        self.block.raw_stringtable()
    }
}

/// An OpenStreetMap way element (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Way)).
///
/// A way contains an ordered list of node references that can be accessed with the `refs` or the
/// `raw_refs` method.
#[derive(Clone, Debug)]
pub struct Way<'a> {
    block: &'a PrimitiveBlock,
    id: i64,
    keys: &'a [u8],
    vals: &'a [u8],
    info: Option<&'a [u8]>,
    /// Packed, delta coded node references.
    refs: &'a [u8],
    /// Packed, delta coded node latitudes (only with `LocationsOnWays`).
    lat: &'a [u8],
    /// Packed, delta coded node longitudes (only with `LocationsOnWays`).
    lon: &'a [u8],
}

impl<'a> Way<'a> {
    /// Decodes one `Way` message.
    pub(crate) fn from_wire(block: &'a PrimitiveBlock, data: &'a [u8]) -> Way<'a> {
        let mut way = Way {
            block,
            id: 0,
            keys: &[],
            vals: &[],
            info: None,
            refs: &[],
            lat: &[],
            lon: &[],
        };

        let mut fields = FieldIter::new(data);
        while let Ok(Some((field, value))) = fields.next_field() {
            match (field, value) {
                (1, WireValue::Varint(value)) => way.id = value as i64,
                (2, WireValue::Bytes(bytes)) => way.keys = bytes,
                (3, WireValue::Bytes(bytes)) => way.vals = bytes,
                (4, WireValue::Bytes(bytes)) => way.info = Some(bytes),
                (8, WireValue::Bytes(bytes)) => way.refs = bytes,
                (9, WireValue::Bytes(bytes)) => way.lat = bytes,
                (10, WireValue::Bytes(bytes)) => way.lon = bytes,
                _ => {}
            }
        }

        way
    }

    /// Returns the way id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Returns an iterator over the tags of this way
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of strings (key and value).
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = ElementReader::from_path("tests/test.osm.pbf")?;
    ///
    /// reader.for_each(|element| {
    ///     if let Element::Way(way) = element {
    ///         for (key, value) in way.tags() {
    ///             println!("key: {key}, value: {value}");
    ///         }
    ///     }
    /// })?;
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            block: self.block,
            keys: PackedVarints::plain(self.keys),
            vals: PackedVarints::plain(self.vals),
        }
    }

    /// Returns additional metadata for this element.
    pub fn info(&self) -> Info<'a> {
        Info::from_wire(self.block, self.info.unwrap_or(&[]))
    }

    /// Returns an iterator over the references of this way. Each reference should correspond to a
    /// node id.
    ///
    /// Finding the corresponding node might involve iterating over the whole PBF structure, but
    /// (to save space) ways themselves usually do not contain geo coordinates.
    pub fn refs(&self) -> WayRefIter<'a> {
        WayRefIter {
            deltas: PackedVarints::delta(self.refs),
        }
    }

    /// Returns an iterator over the way's node locations (latitude, longitude).
    /// Only available if the optional `LocationsOnWays` feature is included in the
    /// [`HeaderBlock`](crate::block::HeaderBlock) and should return an empty iterator otherwise
    /// (See the [`optional_features`](crate::block::HeaderBlock::optional_features) method).
    ///
    /// Use [`refs`](Way::refs) if this feature is not present or to get information other than
    /// coordinates about the nodes that constitute a way.
    pub fn node_locations(&self) -> WayNodeLocationsIter<'a> {
        WayNodeLocationsIter {
            block: self.block,
            dlats: PackedVarints::delta(self.lat),
            dlons: PackedVarints::delta(self.lon),
        }
    }

    /// Returns an iterator over the delta coded node ids of this way.
    ///
    /// The values are not accumulated; use [`refs`](Way::refs) for absolute ids.
    pub fn raw_refs(&self) -> impl Iterator<Item = i64> + 'a {
        PackedVarints::zigzag(self.refs)
    }

    /// Returns an iterator over the tags of this way
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of indices (key and value) to the stringtable of the current
    /// [`PrimitiveBlock`].
    pub fn raw_tags(&self) -> RawTagIter<'a> {
        RawTagIter {
            keys: PackedVarints::plain(self.keys),
            vals: PackedVarints::plain(self.vals),
        }
    }

    /// Returns the raw stringtable. Elements in a `PrimitiveBlock` do not store strings
    /// themselves; instead, they just store indices to a common stringtable. By convention, the
    /// contained strings are UTF-8 encoded but it is not safe to assume that (use
    /// `std::str::from_utf8`).
    pub fn raw_stringtable(&self) -> &[Vec<u8>] {
        self.block.raw_stringtable()
    }
}

/// An OpenStreetMap relation element (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Relation)).
///
/// A relation contains an ordered list of members that can be of any element type.
#[derive(Clone, Debug)]
pub struct Relation<'a> {
    block: &'a PrimitiveBlock,
    id: i64,
    keys: &'a [u8],
    vals: &'a [u8],
    info: Option<&'a [u8]>,
    roles_sid: &'a [u8],
    memids: &'a [u8],
    types: &'a [u8],
}

impl<'a> Relation<'a> {
    /// Decodes one `Relation` message.
    pub(crate) fn from_wire(block: &'a PrimitiveBlock, data: &'a [u8]) -> Relation<'a> {
        let mut relation = Relation {
            block,
            id: 0,
            keys: &[],
            vals: &[],
            info: None,
            roles_sid: &[],
            memids: &[],
            types: &[],
        };

        let mut fields = FieldIter::new(data);
        while let Ok(Some((field, value))) = fields.next_field() {
            match (field, value) {
                (1, WireValue::Varint(value)) => relation.id = value as i64,
                (2, WireValue::Bytes(bytes)) => relation.keys = bytes,
                (3, WireValue::Bytes(bytes)) => relation.vals = bytes,
                (4, WireValue::Bytes(bytes)) => relation.info = Some(bytes),
                (8, WireValue::Bytes(bytes)) => relation.roles_sid = bytes,
                (9, WireValue::Bytes(bytes)) => relation.memids = bytes,
                (10, WireValue::Bytes(bytes)) => relation.types = bytes,
                _ => {}
            }
        }

        relation
    }

    /// Returns the relation id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Returns an iterator over the tags of this relation
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of strings (key and value).
    ///
    /// # Example
    /// ```
    /// use osmpbf::*;
    ///
    /// # fn foo() -> Result<()> {
    /// let reader = ElementReader::from_path("tests/test.osm.pbf")?;
    ///
    /// reader.for_each(|element| {
    ///     if let Element::Relation(relation) = element {
    ///         for (key, value) in relation.tags() {
    ///             println!("key: {key}, value: {value}");
    ///         }
    ///     }
    /// })?;
    ///
    /// # Ok(())
    /// # }
    /// # foo().unwrap();
    /// ```
    pub fn tags(&self) -> TagIter<'a> {
        TagIter {
            block: self.block,
            keys: PackedVarints::plain(self.keys),
            vals: PackedVarints::plain(self.vals),
        }
    }

    /// Returns additional metadata for this element.
    pub fn info(&self) -> Info<'a> {
        Info::from_wire(self.block, self.info.unwrap_or(&[]))
    }

    /// Returns an iterator over the members of this relation.
    pub fn members(&self) -> RelMemberIter<'a> {
        RelMemberIter {
            block: self.block,
            roles: PackedVarints::plain(self.roles_sid),
            memids: PackedVarints::delta(self.memids),
            types: PackedVarints::plain(self.types),
        }
    }

    /// Returns an iterator over the tags of this relation
    /// (See [OSM wiki](http://wiki.openstreetmap.org/wiki/Tags)).
    /// A tag is represented as a pair of indices (key and value) to the stringtable of the current
    /// [`PrimitiveBlock`].
    pub fn raw_tags(&self) -> RawTagIter<'a> {
        RawTagIter {
            keys: PackedVarints::plain(self.keys),
            vals: PackedVarints::plain(self.vals),
        }
    }

    /// Returns the raw stringtable. Elements in a `PrimitiveBlock` do not store strings
    /// themselves; instead, they just store indices to a common stringtable. By convention, the
    /// contained strings are UTF-8 encoded but it is not safe to assume that (use
    /// `std::str::from_utf8`).
    pub fn raw_stringtable(&self) -> &[Vec<u8>] {
        self.block.raw_stringtable()
    }
}

/// An iterator over the references of a way.
///
/// Each reference corresponds to a node id.
#[derive(Clone, Debug)]
pub struct WayRefIter<'a> {
    deltas: PackedVarints<'a>,
}

impl Iterator for WayRefIter<'_> {
    type Item = i64;

    fn next(&mut self) -> Option<Self::Item> {
        self.deltas.next_value()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Each value takes at least one byte.
        (0, Some(self.deltas.upper_bound()))
    }
}

pub struct WayNodeLocation {
    lat: i64,
    lon: i64,
}

/// A node location that contains latitude and longitude coordinates.
impl WayNodeLocation {
    /// Returns the latitude coordinate in degrees.
    pub fn lat(&self) -> f64 {
        1e-9 * self.nano_lat() as f64
    }

    /// Returns the latitude coordinate in nanodegrees (10⁻⁹).
    pub fn nano_lat(&self) -> i64 {
        self.lat
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
        self.lon
    }

    /// Returns the longitude coordinate in decimicrodegrees (10⁻⁷).
    pub fn decimicro_lon(&self) -> i32 {
        (self.nano_lon() / 100) as i32
    }
}

/// An iterator over the node locations of a way.
/// Each element is a pair of coordinates consisting of latitude and longitude.
#[derive(Clone, Debug)]
pub struct WayNodeLocationsIter<'a> {
    block: &'a PrimitiveBlock,
    dlats: PackedVarints<'a>,
    dlons: PackedVarints<'a>,
}

impl Iterator for WayNodeLocationsIter<'_> {
    type Item = WayNodeLocation;

    fn next(&mut self) -> Option<Self::Item> {
        let lat = self.dlats.next_value()?;
        let lon = self.dlons.next_value()?;
        Some(WayNodeLocation {
            lat: self.block.lat_offset() + i64::from(self.block.granularity()) * lat,
            lon: self.block.lon_offset() + i64::from(self.block.granularity()) * lon,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let upper = self.dlats.upper_bound().min(self.dlons.upper_bound());
        (0, Some(upper))
    }
}

/// The element type of a relation member.
///
/// This enum is `#[non_exhaustive]`: the OpenStreetMap data model may grow new
/// member types, and files are not trusted to only contain the ones this crate
/// knows about. Match arms should therefore end with a `_ =>` arm.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RelMemberType {
    /// The member is a node.
    Node,
    /// The member is a way.
    Way,
    /// The member is a relation.
    Relation,
    /// A member type this crate does not know, containing the raw value from
    /// the file. It is returned instead of panicking on malformed or future
    /// data.
    Unknown(i32),
}

impl RelMemberType {
    /// Maps the raw enum value from the file to a member type.
    pub(crate) fn from_raw(value: i32) -> RelMemberType {
        match value {
            0 => RelMemberType::Node,
            1 => RelMemberType::Way,
            2 => RelMemberType::Relation,
            other => RelMemberType::Unknown(other),
        }
    }
}

//TODO encapsulate member_id based on member_type (NodeId, WayId, RelationId)
/// A member of a relation.
///
/// Each member has a member type and a member id that references an element of that type.
#[derive(Clone, Debug)]
pub struct RelMember<'a> {
    block: &'a PrimitiveBlock,
    pub role_sid: i32,
    pub member_id: i64,
    pub member_type: RelMemberType,
}

impl<'a> RelMember<'a> {
    /// Returns the role of a relation member.
    pub fn role(&self) -> Result<&'a str> {
        str_from_stringtable(self.block, self.role_sid as usize)
    }
}

/// An iterator over the members of a relation.
#[derive(Clone, Debug)]
pub struct RelMemberIter<'a> {
    block: &'a PrimitiveBlock,
    roles: PackedVarints<'a>,
    memids: PackedVarints<'a>,
    types: PackedVarints<'a>,
}

impl<'a> Iterator for RelMemberIter<'a> {
    type Item = RelMember<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let role_sid = self.roles.next_value()? as i32;
        let member_id = self.memids.next_value()?;
        let member_type = RelMemberType::from_raw(self.types.next_value()? as i32);
        Some(RelMember {
            block: self.block,
            role_sid,
            member_id,
            member_type,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let upper = self
            .roles
            .upper_bound()
            .min(self.memids.upper_bound())
            .min(self.types.upper_bound());
        (0, Some(upper))
    }
}

/// An iterator over the tags of an element. It returns a pair of strings (key and value).
///
/// Iteration is best effort: a tag whose key or value index is out of bounds,
/// or whose string is not valid UTF-8, is skipped instead of ending the
/// iteration. Use the element's `raw_tags` method to inspect the indices and
/// string table directly if such data must be handled exactly.
#[derive(Clone, Debug)]
pub struct TagIter<'a> {
    block: &'a PrimitiveBlock,
    keys: PackedVarints<'a>,
    vals: PackedVarints<'a>,
}

impl<'a> Iterator for TagIter<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let key_index = self.keys.next_value()? as usize;
            let val_index = self.vals.next_value()? as usize;
            if let Some(tag) =
                get_stringtable_key_value(self.block, Some(key_index), Some(val_index))
            {
                return Some(tag);
            }
            // Skip the malformed tag; returning `None` here would silently end
            // the iteration and drop every remaining tag of this element.
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Malformed entries are skipped, so only the upper bound is known.
        let upper = self.keys.upper_bound().min(self.vals.upper_bound());
        (0, Some(upper))
    }
}

/// An iterator over the tags of an element. It returns a pair of indices (key and value) to the
/// stringtable of the current [`PrimitiveBlock`].
#[derive(Clone, Debug)]
pub struct RawTagIter<'a> {
    keys: PackedVarints<'a>,
    vals: PackedVarints<'a>,
}

impl Iterator for RawTagIter<'_> {
    type Item = (u32, u32);

    fn next(&mut self) -> Option<Self::Item> {
        let key_index = self.keys.next_value()? as u32;
        let val_index = self.vals.next_value()? as u32;
        Some((key_index, val_index))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let upper = self.keys.upper_bound().min(self.vals.upper_bound());
        (0, Some(upper))
    }
}

/// Additional metadata that might be included in each element.
#[derive(Clone, Debug)]
pub struct Info<'a> {
    block: &'a PrimitiveBlock,
    version: Option<i32>,
    timestamp: Option<i64>,
    changeset: Option<i64>,
    uid: Option<i32>,
    user_sid: Option<u32>,
    visible: Option<bool>,
}

impl<'a> Info<'a> {
    /// Decodes an `Info` message; an empty slice yields "nothing set".
    fn from_wire(block: &'a PrimitiveBlock, data: &'a [u8]) -> Info<'a> {
        let mut info = Info {
            block,
            version: None,
            timestamp: None,
            changeset: None,
            uid: None,
            user_sid: None,
            visible: None,
        };

        let mut fields = FieldIter::new(data);
        while let Ok(Some((field, value))) = fields.next_field() {
            if let WireValue::Varint(value) = value {
                match field {
                    1 => info.version = Some(value as i32),
                    2 => info.timestamp = Some(value as i64),
                    3 => info.changeset = Some(value as i64),
                    4 => info.uid = Some(value as i32),
                    5 => info.user_sid = Some(value as u32),
                    6 => info.visible = Some(value != 0),
                    _ => {}
                }
            }
        }

        info
    }

    /// Returns the version of this element.
    pub fn version(&self) -> Option<i32> {
        self.version
    }

    /// Returns the time stamp in milliseconds since the epoch.
    pub fn milli_timestamp(&self) -> Option<i64> {
        self.timestamp
            .map(|timestamp| timestamp * i64::from(self.block.date_granularity()))
    }

    /// Returns the changeset id.
    pub fn changeset(&self) -> Option<i64> {
        self.changeset
    }

    /// Returns the user id.
    pub fn uid(&self) -> Option<i32> {
        self.uid
    }

    /// Returns the user name.
    pub fn user(&self) -> Option<Result<&'a str>> {
        self.user_sid
            .map(|user_sid| str_from_stringtable(self.block, user_sid as usize))
    }

    /// Returns the visibility status of an element. This is only relevant if the PBF file contains
    /// historical information.
    pub fn visible(&self) -> bool {
        // If the visible flag is not present it must be assumed to be true.
        self.visible.unwrap_or(true)
    }

    /// Returns true if the element was deleted.
    /// This is a convenience function that just returns the inverse of [`Info::visible`].
    pub fn deleted(&self) -> bool {
        !self.visible()
    }
}
