//! A field declared `f64` holds a float however the document was written. Index keys order every integer before
//! every float (`keys`), so a whole number stored as a JSON integer (`5` rather than `5.0`: what a JSON client or an
//! integer literal in a patch sends, and what the type accepts) would sit apart from the floats: an `eq` or a range
//! on the typed value would miss it, and so would the field's aggregates and search filters. Untyped ranges (the
//! dashboard's filters) meet the same fields from the other side.

use rxferrix::testing::TestApp;
use serde_json::{Value, json};

pub mod readings {
    use rxferrix::prelude::*;
    use serde_json::Value;

    #[object]
    #[derive(Clone, Debug)]
    pub struct Span {
        pub low: f64,
        pub high: Option<f64>,
    }

    #[table(name = "float_readings")]
    #[index(by_value = [value])]
    #[search_index(search_note, search = note, filter = [value])]
    #[aggregate(per_value, group = [value], sum = [weight])]
    pub struct Reading {
        pub note: String,
        pub value: f64,
        pub weight: f64,
        pub samples: Vec<f64>,
        pub span: Span,
        pub limit: Option<f64>,
        /// Not a float: whole numbers stay as they are.
        pub count: i64,
    }

    #[internal_mutation]
    pub async fn insert_raw(ctx: &MutationCtx, doc: Value) -> Result<String> {
        ctx.db.insert_raw("float_readings", doc).await
    }

    #[internal_mutation]
    pub async fn replace_raw(ctx: &MutationCtx, id: String, doc: Value) -> Result<()> {
        ctx.db.replace_raw(&id, doc).await
    }

    #[internal_mutation]
    pub async fn patch(ctx: &MutationCtx, id: Id<Reading>, patch: Value) -> Result<()> {
        ctx.db.patch(id, patch).await
    }

    #[internal_query]
    pub async fn stored(ctx: &QueryCtx, id: String) -> Result<Value> {
        Ok(ctx.db.get_raw(&id).await?.expect("exists").1)
    }

    fn notes(docs: Vec<Doc<Reading>>) -> Vec<String> {
        docs.into_iter().map(|d| d.value.note).collect()
    }

    #[internal_query]
    pub async fn at(ctx: &QueryCtx, value: f64) -> Result<Vec<String>> {
        Ok(notes(
            ctx.db.query::<Reading>().with_index(Reading::by_value, |r| r.eq(Reading::value, value)).collect().await?,
        ))
    }

    #[internal_query]
    pub async fn at_least(ctx: &QueryCtx, value: f64) -> Result<Vec<String>> {
        Ok(notes(
            ctx.db.query::<Reading>().with_index(Reading::by_value, |r| r.gte(Reading::value, value)).collect().await?,
        ))
    }

    #[internal_query]
    pub async fn below(ctx: &QueryCtx, value: f64) -> Result<Vec<String>> {
        Ok(notes(
            ctx.db.query::<Reading>().with_index(Reading::by_value, |r| r.lt(Reading::value, value)).collect().await?,
        ))
    }

    /// As the dashboard filters: the bound is whatever JSON was typed.
    #[internal_query]
    pub async fn raw_at_least(ctx: &QueryCtx, bound: Value) -> Result<usize> {
        Ok(ctx
            .db
            .query_table("float_readings")
            .with_index("by_value", |r| r.gte("value", bound))
            .collect()
            .await?
            .len())
    }

    #[internal_query]
    pub async fn raw_created_after(ctx: &QueryCtx, bound: Value) -> Result<usize> {
        let q = ctx.db.query_table("float_readings").with_index("by_creation_time", |r| r.gt("_creationTime", bound));
        Ok(q.collect().await?.len())
    }

    #[internal_query]
    pub async fn count_at(ctx: &QueryCtx, value: f64) -> Result<u64> {
        Ok(ctx.db.aggregate(Reading::per_value).get(|g| g.eq(Reading::value, value)).await?.count)
    }

    #[internal_query]
    pub async fn found(ctx: &QueryCtx, text: String, value: f64) -> Result<Vec<String>> {
        let q = ctx
            .db
            .query::<Reading>()
            .with_search_index(Reading::search_note, |q| q.search(Reading::note, text).eq(Reading::value, value));
        Ok(notes(q.collect().await?))
    }
}

use readings::*;

/// A document as a JSON client writes it: whole numbers without a fraction.
fn reading(note: &str, value: i64) -> Value {
    json!({
        "note": note, "value": value, "weight": 2, "samples": [1, 2.5], "span": { "low": 0, "high": 10 }, "limit": 3,
        "count": 4,
    })
}

async fn assert_found_at(t: &TestApp, value: f64, note: &str) {
    let notes = vec![note.to_string()];
    assert_eq!(t.query::<at>((value,)).await.unwrap(), notes, "eq({value})");
    assert_eq!(t.query::<at_least>((value - 0.5,)).await.unwrap(), notes, "gte({})", value - 0.5);
    assert_eq!(t.query::<below>((value - 0.5,)).await.unwrap(), Vec::<String>::new(), "lt({})", value - 0.5);
    assert_eq!(t.query::<count_at>((value,)).await.unwrap(), 1, "the aggregate's group {value}");
    assert_eq!(t.query::<found>((note.to_string(), value)).await.unwrap(), notes, "search with eq({value})");
}

#[tokio::test]
async fn whole_numbers_in_float_fields_are_stored_as_floats() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<insert_raw>((reading("first", 5),)).await.unwrap();
    let doc = t.query::<stored>((id.clone(),)).await.unwrap();
    for (path, v) in [
        ("value", &doc["value"]),
        ("weight", &doc["weight"]),
        ("samples[0]", &doc["samples"][0]),
        ("span.low", &doc["span"]["low"]),
        ("span.high", &doc["span"]["high"]),
        ("limit", &doc["limit"]),
    ] {
        assert!(v.is_f64(), "{path} is stored as {v}");
    }
    assert!(doc["count"].is_i64(), "an integer field keeps its integer: {}", doc["count"]);
    assert_eq!(doc["value"], json!(5.0));
    assert_found_at(&t, 5.0, "first").await;

    // The untyped replace (the dashboard's save) and a typed patch with an integer literal.
    t.mutation::<replace_raw>((id.clone(), reading("first", 7))).await.unwrap();
    assert_found_at(&t, 7.0, "first").await;
    t.mutation::<patch>((id.parse().unwrap(), json!({ "value": 9, "limit": null }))).await.unwrap();
    assert_found_at(&t, 9.0, "first").await;
    let doc = t.query::<stored>((id,)).await.unwrap();
    assert!(doc["value"].is_f64() && doc["limit"].is_null(), "{doc}");
}

#[tokio::test]
async fn untyped_bounds_on_float_fields_compare_as_floats() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<insert_raw>((reading("first", 5),)).await.unwrap();
    // `9` typed into a filter is an integer; the field's values are floats.
    assert_eq!(t.query::<raw_at_least>((json!(5),)).await.unwrap(), 1);
    assert_eq!(t.query::<raw_at_least>((json!(6),)).await.unwrap(), 0);
    assert_eq!(t.query::<raw_at_least>((json!(4.5),)).await.unwrap(), 1);
    // `_creationTime` is a float too (milliseconds).
    assert_eq!(t.query::<raw_created_after>((json!(0),)).await.unwrap(), 1);
    assert_eq!(t.query::<raw_created_after>((json!(9_000_000_000_000_i64),)).await.unwrap(), 0);
}
