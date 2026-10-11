//! Shapes: what a stored value looks like, as far as its Rust type tells. The analytics copy (`crate::olap`) builds
//! its typed columns from them.
//!
//! `#[table]` and `#[object]` emit the shape of their fields (names as serde writes them: `rename_all`, `rename`,
//! `skip`, `flatten`); everything else implements [`HasShape`] here. A type without an implementation (a plain serde
//! type of the app's own) doesn't fail to compile: it is [`Shape::Json`], kept as JSON. So is anything whose JSON form
//! the type doesn't pin down (`serialize_with`, tagged enums, maps, tuples, 128-bit integers).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::marker::PhantomData;
use std::sync::Arc;

/// The JSON form of a value.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Bool,
    /// A JSON number without a fraction that fits an `i64`.
    Int,
    /// A JSON number without a fraction that fits a `u64`.
    UInt,
    Float,
    /// A JSON string.
    Text,
    /// [`crate::Bytes`]: `{"$bytes": "<base64>"}`.
    Bytes,
    /// `null` or the inner shape (a missing field reads as `null` too).
    Optional(Box<Shape>),
    List(Box<Shape>),
    Struct(StructShape),
    /// Anything: kept as JSON.
    Json,
}

/// The fields of a JSON object.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StructShape {
    /// `(JSON name, shape)`, in declaration order.
    pub fields: Vec<(String, Shape)>,
    /// Keys beyond `fields` may appear (a `#[serde(flatten)]` of something that isn't a struct, or a field whose JSON
    /// name the macro couldn't tell).
    pub open: bool,
}

impl StructShape {
    pub fn field(&mut self, name: &str, shape: Shape) {
        self.fields.push((name.to_string(), shape));
    }

    /// A field that is left out when empty (`skip_serializing_if`): it may be missing.
    pub fn optional_field(&mut self, name: &str, shape: Shape) {
        self.field(name, shape.optional());
    }

    /// `#[serde(flatten)]`: a struct's fields join these; anything else makes the object open.
    pub fn flatten(&mut self, shape: Shape) {
        match shape {
            Shape::Struct(inner) => {
                self.open |= inner.open;
                self.fields.extend(inner.fields);
            }
            Shape::Optional(inner) => match *inner {
                Shape::Struct(inner) => {
                    self.open |= inner.open;
                    self.fields.extend(inner.fields.into_iter().map(|(n, s)| (n, s.optional())));
                }
                _ => self.open = true,
            },
            _ => self.open = true,
        }
    }

    pub fn open(&mut self) {
        self.open = true;
    }
}

impl Shape {
    /// `Optional(self)`, without nesting optionals.
    pub fn optional(self) -> Shape {
        match self {
            Shape::Optional(_) | Shape::Json => self,
            other => Shape::Optional(Box::new(other)),
        }
    }

    /// For tables whose expansion was written by hand (tests): no known fields.
    pub fn unknown() -> Shape {
        Shape::Struct(StructShape { fields: Vec::new(), open: true })
    }

    /// A stable text form, for fingerprints.
    pub fn fingerprint(&self) -> String {
        format!("{self:?}")
    }
}

/// Types whose JSON form is known.
pub trait HasShape {
    fn shape() -> Shape;
}

macro_rules! shapes {
    ($shape:expr => $($t:ty),* $(,)?) => {
        $(impl HasShape for $t {
            fn shape() -> Shape {
                $shape
            }
        })*
    };
}
shapes!(Shape::Bool => bool);
shapes!(Shape::Int => i8, i16, i32, i64, isize, u8, u16, u32);
shapes!(Shape::UInt => u64, usize);
shapes!(Shape::Float => f32, f64);
shapes!(Shape::Text => String, str, char);
shapes!(Shape::Bytes => crate::Bytes);
shapes!(Shape::Json => serde_json::Value, serde_json::Map<String, serde_json::Value>, u128, i128, ());

impl<T> HasShape for crate::Id<T> {
    fn shape() -> Shape {
        Shape::Text
    }
}

impl<T: HasShape> HasShape for Option<T> {
    fn shape() -> Shape {
        T::shape().optional()
    }
}

macro_rules! wrappers {
    ($($w:ident),*) => {
        $(impl<T: HasShape + ?Sized> HasShape for $w<T> {
            fn shape() -> Shape {
                T::shape()
            }
        })*
    };
}
wrappers!(Box, Arc);

impl<T: HasShape + ?Sized> HasShape for &T {
    fn shape() -> Shape {
        T::shape()
    }
}

macro_rules! lists {
    ($($l:ident),*) => {
        $(impl<T: HasShape> HasShape for $l<T> {
            fn shape() -> Shape {
                Shape::List(Box::new(T::shape()))
            }
        })*
    };
}
lists!(Vec, VecDeque, BTreeSet);

impl<T: HasShape, S> HasShape for HashSet<T, S> {
    fn shape() -> Shape {
        Shape::List(Box::new(T::shape()))
    }
}

impl<T: HasShape> HasShape for [T] {
    fn shape() -> Shape {
        Shape::List(Box::new(T::shape()))
    }
}

impl<T: HasShape, const N: usize> HasShape for [T; N] {
    fn shape() -> Shape {
        Shape::List(Box::new(T::shape()))
    }
}

impl<K, V, S> HasShape for HashMap<K, V, S> {
    fn shape() -> Shape {
        Shape::Json
    }
}

impl<K, V> HasShape for BTreeMap<K, V> {
    fn shape() -> Shape {
        Shape::Json
    }
}

/// A document: the system fields, then the table's.
impl<T: crate::Table> HasShape for crate::Doc<T> {
    fn shape() -> Shape {
        let mut s = StructShape::default();
        s.field("_id", Shape::Text);
        s.field("_creationTime", Shape::Float);
        s.flatten((T::table_def().shape)());
        Shape::Struct(s)
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// For the macros: the shape of any type, `Json` when it has no `HasShape` (autoref specialization: the macros call
// `(&&ShapeProbe::<T>::new()).probe_shape()`, which resolves to `ProbeHasShape` when `T: HasShape`, else falls back to
// `ProbeFallback` one auto-deref later).
// ---------------------------------------------------------------------------------------------------------------------

#[doc(hidden)]
pub struct ShapeProbe<T: ?Sized>(PhantomData<fn() -> PhantomData<T>>);

impl<T: ?Sized> ShapeProbe<T> {
    #[doc(hidden)]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        ShapeProbe(PhantomData)
    }
}

#[doc(hidden)]
pub trait ProbeHasShape {
    fn probe_shape(&self) -> Shape;
}

impl<T: HasShape + ?Sized> ProbeHasShape for &ShapeProbe<T> {
    fn probe_shape(&self) -> Shape {
        T::shape()
    }
}

#[doc(hidden)]
pub trait ProbeFallback {
    fn probe_shape(&self) -> Shape;
}

impl<T: ?Sized> ProbeFallback for ShapeProbe<T> {
    fn probe_shape(&self) -> Shape {
        Shape::Json
    }
}

thread_local! {
    static COMPUTING: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Compute a type's shape, or `Json` if that type is already being computed on this thread (a recursive type).
#[doc(hidden)]
pub fn guard(type_name: &'static str, f: impl FnOnce() -> Shape) -> Shape {
    if COMPUTING.with(|c| c.borrow().contains(&type_name)) {
        return Shape::Json;
    }
    COMPUTING.with(|c| c.borrow_mut().push(type_name));
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            COMPUTING.with(|c| c.borrow_mut().pop());
        }
    }
    let _pop = Pop;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoShape;

    #[test]
    #[allow(clippy::needless_borrow)] // the double borrow selects the fallback
    fn known_types_and_the_fallback() {
        assert_eq!((&&ShapeProbe::<i64>::new()).probe_shape(), Shape::Int);
        assert_eq!((&&ShapeProbe::<Option<Vec<String>>>::new()).probe_shape(), Shape::Optional(Box::new(Shape::List(Box::new(Shape::Text)))));
        assert_eq!((&&ShapeProbe::<Option<Option<f64>>>::new()).probe_shape(), Shape::Optional(Box::new(Shape::Float)));
        assert_eq!((&&ShapeProbe::<NoShape>::new()).probe_shape(), Shape::Json);
        assert_eq!((&&ShapeProbe::<Vec<NoShape>>::new()).probe_shape(), Shape::Json, "a list of unknowns is JSON as a whole");
        assert_eq!((&&ShapeProbe::<u64>::new()).probe_shape(), Shape::UInt);
        assert_eq!((&&ShapeProbe::<i128>::new()).probe_shape(), Shape::Json, "fits no 64-bit column");
        assert_eq!((&&ShapeProbe::<crate::Id<NoShape>>::new()).probe_shape(), Shape::Text);
    }

    #[test]
    fn flatten_merges_structs_and_opens_on_anything_else() {
        let mut inner = StructShape::default();
        inner.field("a", Shape::Int);
        let mut s = StructShape::default();
        s.field("x", Shape::Bool);
        s.flatten(Shape::Struct(inner.clone()));
        assert_eq!(s.fields.len(), 2);
        assert!(!s.open);
        s.flatten(Shape::Optional(Box::new(Shape::Struct(inner))));
        assert_eq!(s.fields[2], ("a".into(), Shape::Optional(Box::new(Shape::Int))));
        s.flatten(Shape::Json);
        assert!(s.open);
    }

    #[test]
    fn recursion_stops_at_the_guard() {
        fn shape() -> Shape {
            guard("Tree", || {
                let mut s = StructShape::default();
                s.field("children", Shape::List(Box::new(shape())));
                Shape::Struct(s)
            })
        }
        let Shape::Struct(s) = shape() else { panic!() };
        assert_eq!(s.fields[0].1, Shape::List(Box::new(Shape::Json)));
    }
}
