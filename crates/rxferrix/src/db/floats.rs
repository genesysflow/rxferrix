//! Where a table's documents hold floats, so that a whole number written there as a JSON integer (`5` for an `f64`
//! field: what a JSON client sends, and what the type accepts) is stored as the float it is (`5.0`).
//!
//! Index keys carry the JSON type of each value and order every integer before every float ([`crate::keys`]). A
//! field declared `f64` must therefore always hold a float: an integer in it would sit apart from the other values
//! of its index, where no typed `eq` or range (which serialize an `f64`) finds it. The same goes for its aggregates'
//! groups and search filters. Untyped bounds meet these fields from the other side ([`Floats::bound`]).

use std::borrow::Cow;

use serde_json::{Map, Number, Value};

use crate::schema::CREATION_TIME_FIELD;
use crate::shape::Shape;

/// The places of a value that hold floats, as far as its shape tells.
#[derive(Clone, Debug, PartialEq)]
pub enum Floats {
    /// The value itself.
    Here,
    /// Every item of a list.
    Each(Box<Floats>),
    /// These fields of an object.
    Fields(Vec<(String, Floats)>),
}

impl Floats {
    /// The places of `shape` that hold floats, if any. Optional ones count (`null` is left alone); what a shape
    /// doesn't pin down (`Shape::Json`, the unknown keys of an open struct) doesn't.
    pub fn of(shape: &Shape) -> Option<Floats> {
        match shape {
            Shape::Float => Some(Floats::Here),
            Shape::Optional(inner) => Floats::of(inner),
            Shape::List(inner) => Floats::of(inner).map(|f| Floats::Each(Box::new(f))),
            Shape::Struct(s) => {
                let fields: Vec<(String, Floats)> =
                    s.fields.iter().filter_map(|(name, shape)| Some((name.clone(), Floats::of(shape)?))).collect();
                (!fields.is_empty()).then_some(Floats::Fields(fields))
            }
            _ => None,
        }
    }

    /// Make the integers in these places of `value` floats. Anything else there (a string where the type wants a
    /// number) is left for validation to report.
    pub fn apply(&self, value: &mut Value) {
        match (self, value) {
            (Floats::Here, Value::Number(n)) => to_float(n),
            (Floats::Each(inner), Value::Array(items)) => items.iter_mut().for_each(|item| inner.apply(item)),
            (Floats::Fields(fields), Value::Object(object)) => apply_fields(fields, object),
            _ => {}
        }
    }

    /// [`Floats::apply`] to a document.
    pub fn apply_to_doc(&self, doc: &mut Map<String, Value>) {
        if let Floats::Fields(fields) = self {
            apply_fields(fields, doc);
        }
    }

    /// Is the document field `name` a float? `_creationTime` is one in every table.
    fn is_float_field(floats: Option<&Floats>, name: &str) -> bool {
        name == CREATION_TIME_FIELD
            || matches!(floats, Some(Floats::Fields(fields))
                if fields.iter().any(|(field, at)| field == name && *at == Floats::Here))
    }

    /// The value to compare the document field `field` with, given `bound` (a range bound, a group or a filter
    /// value): an integer bound on a float field is that float, since the field's values are floats.
    pub fn bound<'a>(floats: Option<&Floats>, field: &str, bound: &'a Value) -> Cow<'a, Value> {
        match bound {
            Value::Number(n) if !n.is_f64() && Floats::is_float_field(floats, field) => {
                let mut n = n.clone();
                to_float(&mut n);
                Cow::Owned(Value::Number(n))
            }
            _ => Cow::Borrowed(bound),
        }
    }
}

fn apply_fields(fields: &[(String, Floats)], object: &mut Map<String, Value>) {
    for (name, inner) in fields {
        if let Some(value) = object.get_mut(name) {
            inner.apply(value);
        }
    }
}

/// `n` as a float (the nearest one, for an integer beyond 2^53: what reading it as an `f64` gives too).
fn to_float(n: &mut Number) {
    if !n.is_f64()
        && let Some(f) = n.as_f64().and_then(Number::from_f64)
    {
        *n = f;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::shape::StructShape;

    fn shape() -> Shape {
        let mut inner = StructShape::default();
        inner.field("low", Shape::Float);
        inner.field("label", Shape::Text);
        let mut s = StructShape::default();
        s.field("value", Shape::Float);
        s.field("count", Shape::Int);
        s.field("limit", Shape::Float.optional());
        s.field("samples", Shape::List(Box::new(Shape::Float)));
        s.field("spans", Shape::List(Box::new(Shape::Struct(inner))).optional());
        s.field("anything", Shape::Json);
        Shape::Struct(s)
    }

    #[test]
    fn integers_in_float_places_become_floats() {
        let floats = Floats::of(&shape()).unwrap();
        let mut doc = json!({
            "value": 5, "count": 5, "limit": 3, "samples": [1, 2.5, -0, 9007199254740993_i64],
            "spans": [{ "low": 0, "label": "a" }, { "low": 1.5, "label": "b" }], "anything": 7, "unknown": 8,
        });
        floats.apply(&mut doc);
        let text = serde_json::to_string(&doc).unwrap();
        assert!(text.contains(r#""value":5.0"#) && text.contains(r#""count":5,"#), "{text}");
        assert!(
            text.contains(r#""limit":3.0"#) && text.contains(r#""samples":[1.0,2.5,0.0,9007199254740992.0]"#),
            "{text}"
        );
        assert!(text.contains(r#""low":0.0"#) && text.contains(r#""low":1.5"#), "{text}");
        assert!(text.contains(r#""anything":7"#) && text.contains(r#""unknown":8"#), "{text}");
    }

    #[test]
    fn what_is_not_a_number_is_left_alone() {
        let floats = Floats::of(&shape()).unwrap();
        let before = json!({ "value": "5", "limit": null, "samples": "none", "spans": null });
        let mut doc = before.clone();
        floats.apply(&mut doc);
        assert_eq!(doc, before);
    }

    #[test]
    fn a_shape_without_floats_has_no_places() {
        let mut s = StructShape::default();
        s.field("name", Shape::Text);
        s.field("tags", Shape::List(Box::new(Shape::Text)));
        assert_eq!(Floats::of(&Shape::Struct(s)), None);
        assert_eq!(Floats::of(&Shape::unknown()), None);
    }

    #[test]
    fn integer_bounds_on_float_fields_are_floats() {
        let floats = Floats::of(&shape());
        let bound = |field: &str, v: Value| Floats::bound(floats.as_ref(), field, &v).into_owned();
        assert!(bound("value", json!(5)).is_f64());
        assert!(bound("limit", json!(5)).is_f64(), "an optional float");
        assert!(bound("_creationTime", json!(5)).is_f64());
        assert!(bound("count", json!(5)).is_i64());
        assert!(bound("samples", json!(5)).is_i64(), "a list of floats is not a float");
        assert_eq!(bound("value", json!("x")), json!("x"));
        assert!(Floats::bound(None, "_creationTime", &json!(5)).is_f64(), "also in a table without a definition");
    }
}
