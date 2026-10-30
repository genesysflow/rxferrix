//! Document ids.
//!
//! An id is 16 raw bytes: `[table_number: u32 BE][12 bytes]`. Its string form is 26 characters of lowercase
//! Crockford base32 without padding. Base32 packs bits most-significant first, so the string order equals the byte
//! order, and the table can be recovered from any id without a lookup.
//!
//! The 12 bytes of a new document's id ([`DocId::new_ordered`]) are the milliseconds since the Unix epoch when it was
//! created (48 bits, big-endian), then 48 random bits: documents and their index entries are stored in id order, so
//! ids that grow with time put each insert next to the previous ones instead of at a random page of a large table.
//! Stored files keep 96 random bits ([`DocId::new_random`]): a file's id is the capability its URL grants.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::LazyLock;

use data_encoding::{Encoding, Specification};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::schema::Table;

/// Length of the string form of an id.
pub const ID_STR_LEN: usize = 26;

static BASE32: LazyLock<Encoding> = LazyLock::new(|| {
    let mut spec = Specification::new();
    spec.symbols.push_str("0123456789abcdefghjkmnpqrstvwxyz");
    spec.check_trailing_bits = true;
    spec.encoding().expect("valid base32 spec")
});

/// An untyped document id (16 raw bytes).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocId(pub [u8; 16]);

impl DocId {
    /// A fresh id in `table_number` for a document created at `now_ms` (ms since the Unix epoch): the time in 48 bits,
    /// then 48 random bits. See the module docs.
    ///
    /// Measured (2026-10-30) against 96 random bits: loading 200k documents with 8 indexes 21.7k -> 31.9k documents/s
    /// and inserts into that table 11.2k -> 15.1k/s (p99 2.9 -> 1.6 ms, `bench_indexes_large_table`); one-insert
    /// mutations from 64 writers 42.4k -> 69.5k/s and 10-insert ones 62.8k -> 96.0k documents/s (`bench_writes`).
    pub fn new_ordered(table_number: u32, now_ms: f64) -> Self {
        let mut raw = [0u8; 16];
        raw[..4].copy_from_slice(&table_number.to_be_bytes());
        let ms = (now_ms.max(0.0) as u64).min((1 << 48) - 1);
        raw[4..10].copy_from_slice(&ms.to_be_bytes()[2..]);
        let random: [u8; 6] = rand::random();
        raw[10..].copy_from_slice(&random);
        DocId(raw)
    }

    /// A fresh id in `table_number` with 96 random bits, unguessable (stored files: see the module docs).
    pub fn new_random(table_number: u32) -> Self {
        let mut raw = [0u8; 16];
        raw[..4].copy_from_slice(&table_number.to_be_bytes());
        let random: [u8; 12] = rand::random();
        raw[4..].copy_from_slice(&random);
        DocId(raw)
    }

    pub fn table_number(&self) -> u32 {
        u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]])
    }

    pub fn encode(&self) -> String {
        BASE32.encode(&self.0)
    }

    pub fn decode(s: &str) -> Result<Self, InvalidId> {
        if s.len() != ID_STR_LEN {
            return Err(InvalidId(s.to_string()));
        }
        let bytes = BASE32.decode(s.as_bytes()).map_err(|_| InvalidId(s.to_string()))?;
        let raw: [u8; 16] = bytes.try_into().map_err(|_| InvalidId(s.to_string()))?;
        Ok(DocId(raw))
    }
}

impl fmt::Debug for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DocId({})", self.encode())
    }
}

impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

/// Error returned when parsing a malformed id string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId(pub String);

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid id: {:?}", self.0)
    }
}
impl std::error::Error for InvalidId {}

/// A typed document id. See the module docs for the format.
pub struct Id<T> {
    raw: DocId,
    _table: PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    pub(crate) fn from_doc_id(raw: DocId) -> Self {
        Id { raw, _table: PhantomData }
    }

    pub(crate) fn doc_id(&self) -> DocId {
        self.raw
    }

    /// The raw 16 bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.raw.0
    }

    /// The table number encoded in the id.
    pub fn table_number(&self) -> u32 {
        self.raw.table_number()
    }

    /// The 26-character string form.
    pub fn as_str(&self) -> String {
        self.raw.encode()
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Id<T> {}
impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Id<T> {}
impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}
impl<T> Hash for Id<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.raw.hash(state)
    }
}
impl<T> fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.raw.encode())
    }
}
impl<T> fmt::Display for Id<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw.encode())
    }
}

/// Validates the format only; the table is checked when the id is used.
impl<T> FromStr for Id<T> {
    type Err = InvalidId;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        DocId::decode(s).map(Id::from_doc_id)
    }
}

impl<T> Serialize for Id<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw.encode())
    }
}

impl<'de, T> Deserialize<'de> for Id<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl<T: Table> ts_rs::TS for Id<T> {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;

    fn name(_: &ts_rs::Config) -> String {
        format!("Id<\"{}\">", T::NAME)
    }

    fn inline(cfg: &ts_rs::Config) -> String {
        Self::name(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_order() {
        for _ in 0..100 {
            let a = DocId::new_random(7);
            let b = DocId::new_random(7);
            let (sa, sb) = (a.encode(), b.encode());
            assert_eq!(sa.len(), ID_STR_LEN);
            assert_eq!(DocId::decode(&sa).unwrap(), a);
            assert_eq!(sa.cmp(&sb), a.cmp(&b), "string order must equal byte order");
            assert_eq!(a.table_number(), 7);
        }
        let max = DocId([0xFF; 16]);
        assert_eq!(DocId::decode(&max.encode()).unwrap(), max);
    }

    #[test]
    fn ordered_ids_follow_the_time() {
        let t = 1_761_800_000_000.0;
        let (a, b, later) = (DocId::new_ordered(7, t), DocId::new_ordered(7, t), DocId::new_ordered(7, t + 1.0));
        assert_eq!((a.table_number(), later.table_number()), (7, 7));
        assert_eq!(a.0[4..10], b.0[4..10], "the same millisecond");
        assert_ne!(a, b, "random below the millisecond");
        assert!(a < later && b < later);
        assert_eq!(DocId::decode(&later.encode()).unwrap(), later);
        // Out-of-range clocks stay in their 48 bits.
        assert_eq!(DocId::new_ordered(7, -5.0).0[4..10], [0; 6]);
        assert_eq!(DocId::new_ordered(7, f64::MAX).0[4..10], [0xFF; 6]);
    }

    #[test]
    fn rejects_bad_strings() {
        assert!(DocId::decode("").is_err());
        assert!(DocId::decode("abc").is_err());
        let good = DocId::new_random(1).encode();
        assert!(DocId::decode(&good.to_uppercase()).is_err());
        let mut bad = good.clone();
        bad.replace_range(0..1, "u"); // 'u' is not in the Crockford alphabet
        assert!(DocId::decode(&bad).is_err());
        // Non-zero trailing bits are rejected so that every id has exactly one string form.
        let mut trailing = good[..25].to_string();
        trailing.push('z');
        assert!(DocId::decode(&trailing).is_err());
    }
}
