//! [`Bytes`]: a binary value in documents, function args and results.
//!
//! JSON has no binary type, so bytes travel and are stored in a tagged form, `{"$bytes": "<base64>"}` (standard
//! alphabet, padded), as docs/CONTRACT.md §1.1 describes. The memcomparable key encoding recognizes that form and gives it
//! its own type tag, ordered between strings and arrays (`crate::keys`), so `Bytes` fields can be indexed and
//! compared byte-wise. In TypeScript it is `{ $bytes: string }`; `@rxferrix/client` converts to and from `Uint8Array`
//! with `bytesToJson` / `bytesFromJson`.

use std::fmt;
use std::ops::{Deref, DerefMut};

use data_encoding::BASE64;
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// The JSON key of the tagged form.
pub const BYTES_KEY: &str = "$bytes";

/// A byte string. Serializes as `{"$bytes": "<base64>"}`; ordered (and indexed) byte-wise.
///
/// ```
/// use rxferrix::Bytes;
/// let b = Bytes::from(vec![0u8, 255]);
/// assert_eq!(serde_json::to_value(&b).unwrap(), serde_json::json!({ "$bytes": "AP8=" }));
/// assert_eq!(&b[..], &[0, 255]);
/// ```
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Bytes {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Bytes(bytes.into())
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    /// The JSON form, `{"$bytes": "<base64>"}`.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert(BYTES_KEY.to_string(), Value::String(BASE64.encode(&self.0)));
        Value::Object(map)
    }

    /// The bytes of a JSON value in the tagged form: an object whose only key is `$bytes`, holding canonical
    /// base64 (padded, no stray bits). `None` for anything else.
    pub fn from_json(v: &Value) -> Option<Bytes> {
        match v {
            Value::Object(map) => decode_tagged(map),
            _ => None,
        }
    }
}

/// See [`Bytes::from_json`].
pub(crate) fn decode_tagged(map: &Map<String, Value>) -> Option<Bytes> {
    if map.len() != 1 {
        return None;
    }
    match map.get(BYTES_KEY)? {
        Value::String(s) => BASE64.decode(s.as_bytes()).ok().map(Bytes),
        _ => None,
    }
}

impl Deref for Bytes {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.0
    }
}

impl DerefMut for Bytes {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.0
    }
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Self {
        Bytes(v)
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Self {
        Bytes(v.to_vec())
    }
}

impl<const N: usize> From<[u8; N]> for Bytes {
    fn from(v: [u8; N]) -> Self {
        Bytes(v.to_vec())
    }
}

impl From<Bytes> for Vec<u8> {
    fn from(b: Bytes) -> Self {
        b.0
    }
}

/// So that e.g. `ctx.storage.store(bytes, ..)` accepts a `rxferrix::Bytes`.
impl From<Bytes> for axum::body::Bytes {
    fn from(b: Bytes) -> Self {
        axum::body::Bytes::from(b.0)
    }
}

impl From<axum::body::Bytes> for Bytes {
    fn from(b: axum::body::Bytes) -> Self {
        Bytes(b.to_vec())
    }
}

impl fmt::Debug for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Bytes({})", data_encoding::HEXLOWER.encode(&self.0))
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(BYTES_KEY, &BASE64.encode(&self.0))?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Bytes;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bytes as {\"$bytes\": \"<base64>\"}")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Bytes, A::Error> {
                let mut out: Option<Bytes> = None;
                while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                    if key != BYTES_KEY {
                        return Err(de::Error::custom(format!(
                            "unexpected key `{key}` in bytes (expected only `{BYTES_KEY}`)"
                        )));
                    }
                    if out.is_some() {
                        return Err(de::Error::duplicate_field(BYTES_KEY));
                    }
                    let s: std::borrow::Cow<'de, str> = map.next_value()?;
                    let bytes = BASE64
                        .decode(s.as_bytes())
                        .map_err(|e| de::Error::custom(format!("`{BYTES_KEY}` is not valid base64: {e}")))?;
                    out = Some(Bytes(bytes));
                }
                out.ok_or_else(|| de::Error::missing_field(BYTES_KEY))
            }
        }
        deserializer.deserialize_map(BytesVisitor)
    }
}

impl ts_rs::TS for Bytes {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;

    fn name(_: &ts_rs::Config) -> String {
        "{ $bytes: string }".to_string()
    }

    fn inline(cfg: &ts_rs::Config) -> String {
        Self::name(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_roundtrip() {
        for v in [vec![], vec![0u8], vec![0, 255, 1, 2, 3], (0..=255).collect::<Vec<u8>>()] {
            let b = Bytes(v.clone());
            let j = serde_json::to_value(&b).unwrap();
            assert_eq!(j, b.to_json());
            assert_eq!(Bytes::from_json(&j), Some(b.clone()));
            assert_eq!(serde_json::from_value::<Bytes>(j).unwrap(), b);
        }
        assert_eq!(serde_json::to_string(&Bytes(b"hi".to_vec())).unwrap(), r#"{"$bytes":"aGk="}"#);
    }

    #[test]
    fn rejects_other_shapes() {
        for bad in [
            json!("aGk="),
            json!({}),
            json!({"$bytes": "aGk"}),
            json!({"$bytes": 1}),
            json!({"$bytes": "aGk=", "x": 1}),
        ] {
            assert!(serde_json::from_value::<Bytes>(bad.clone()).is_err(), "{bad}");
            assert_eq!(Bytes::from_json(&bad), None, "{bad}");
        }
        // Non-canonical base64 (stray bits in the last character) is rejected, so each value has one encoding.
        assert!(Bytes::from_json(&json!({"$bytes": "aGl="})).is_none());
    }

    #[test]
    fn inside_a_flattened_struct() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Inner {
            data: Option<Bytes>,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Outer {
            id: u32,
            #[serde(flatten)]
            inner: Inner,
        }
        let v = Outer { id: 1, inner: Inner { data: Some(Bytes(vec![1, 2])) } };
        let j = serde_json::to_value(&v).unwrap();
        assert_eq!(j, json!({"id": 1, "data": {"$bytes": "AQI="}}));
        assert_eq!(serde_json::from_value::<Outer>(j).unwrap(), v);
    }

    #[test]
    fn ts_type() {
        use ts_rs::TS;
        let cfg = crate::private::ts_config();
        assert_eq!(Bytes::inline(cfg), "{ $bytes: string }");
        assert_eq!(<Option<Bytes>>::inline(cfg), "{ $bytes: string } | null");
    }
}
