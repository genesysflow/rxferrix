//! Live aggregates (`#[aggregate]`, `rxferrix::db::aggregate`): values, ranges and listings, a model test against the
//! documents, narrow reactivity, rollbacks, mutations reading their own writes, backfill and redefinition, and
//! verification.

use std::collections::BTreeMap;

use rxferrix::db::Database;
use rxferrix::testing::TestApp;
use rxferrix::{AggregateDef, App, AppOptions, TableDef};
use serde_json::{Value, json};

pub mod fns {
    use rxferrix::prelude::*;
    use serde_json::Value;

    #[table(name = "agg_orders")]
    #[index(by_customer_index = [customer])]
    #[aggregate(by_customer, group = [customer], sum = [amount, quantity])]
    #[aggregate(by_customer_day, group = [customer, day], sum = [amount])]
    #[aggregate(total, sum = [amount])]
    pub struct Sale {
        pub customer: String,
        pub day: String,
        pub amount: f64,
        pub quantity: Option<i64>,
        pub note: String,
    }

    #[internal_mutation]
    pub async fn add(
        ctx: &MutationCtx,
        customer: String,
        day: String,
        amount: f64,
        quantity: Option<i64>,
    ) -> Result<Id<Sale>> {
        ctx.db.insert(Sale { customer, day, amount, quantity, note: String::new() }).await
    }

    #[internal_mutation]
    pub async fn patch(ctx: &MutationCtx, id: Id<Sale>, fields: Value) -> Result<()> {
        ctx.db.patch(id, fields).await
    }

    #[internal_mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Sale>) -> Result<()> {
        ctx.db.delete(id).await
    }

    /// Inserts, then fails: the insert and its aggregate changes roll back together.
    #[internal_mutation]
    pub async fn add_then_fail(ctx: &MutationCtx, customer: String) -> Result<()> {
        ctx.db
            .insert(Sale { customer, day: "d".into(), amount: 100.0, quantity: Some(1), note: String::new() })
            .await?;
        Err(UserError::new("nope", "failing on purpose").into())
    }

    /// A mutation reads its own writes.
    #[internal_mutation]
    pub async fn add_and_count(ctx: &MutationCtx, customer: String) -> Result<u64> {
        ctx.db
            .insert(Sale {
                customer: customer.clone(),
                day: "d".into(),
                amount: 1.0,
                quantity: None,
                note: String::new(),
            })
            .await?;
        Ok(ctx.db.aggregate(Sale::by_customer).get(|g| g.eq(Sale::customer, customer)).await?.count)
    }

    #[object]
    pub struct Totals {
        pub count: u64,
        pub amount: f64,
        pub quantity: Option<i64>,
    }

    #[internal_query]
    pub async fn customer(ctx: &QueryCtx, customer: String) -> Result<Totals> {
        let a = ctx.db.aggregate(Sale::by_customer).get(|g| g.eq(Sale::customer, customer)).await?;
        Ok(Totals { count: a.count, amount: a.sum(Sale::amount), quantity: a.sum_i64(Sale::quantity) })
    }

    /// Public, for live subscriptions over WebSocket (`websocket::aggregate_subscriptions_update_per_group`).
    #[query]
    pub async fn customer_count(ctx: &QueryCtx, customer: String) -> Result<u64> {
        Ok(ctx.db.aggregate(Sale::by_customer).get(|g| g.eq(Sale::customer, customer)).await?.count)
    }

    #[internal_query]
    pub async fn total(ctx: &QueryCtx) -> Result<Totals> {
        let a = ctx.db.aggregate(Sale::total).get(|g| g).await?;
        Ok(Totals { count: a.count, amount: a.sum(Sale::amount), quantity: None })
    }

    /// Days `from..=to` of a customer, combined.
    #[internal_query]
    pub async fn days(ctx: &QueryCtx, customer: String, from: String, to: String) -> Result<Totals> {
        let a = ctx
            .db
            .aggregate(Sale::by_customer_day)
            .sum_over(|g| g.eq(Sale::customer, customer).gte(Sale::day, from).lte(Sale::day, to))
            .await?;
        Ok(Totals { count: a.count, amount: a.sum(Sale::amount), quantity: None })
    }

    /// Every group of `by_customer` / `by_customer_day`: `[group values..., count, amount]`.
    #[internal_query]
    pub async fn groups(ctx: &QueryCtx, daily: bool, desc: bool, limit: u32) -> Result<Vec<Vec<Value>>> {
        let order = if desc { Order::Desc } else { Order::Asc };
        let groups = if daily {
            ctx.db.aggregate(Sale::by_customer_day).groups(|g| g, order, limit as usize).await?
        } else {
            ctx.db.aggregate(Sale::by_customer).groups(|g| g, order, limit as usize).await?
        };
        Ok(groups
            .into_iter()
            .map(|g| {
                let mut row = g.group;
                row.push(g.value.count.into());
                row.push(g.value.sum(Sale::amount).into());
                row
            })
            .collect())
    }

    #[internal_query]
    pub async fn bad_get(ctx: &QueryCtx) -> Result<u64> {
        Ok(ctx.db.aggregate(Sale::by_customer_day).get(|g| g.eq(Sale::customer, "a".to_string())).await?.count)
    }
}

use fns::*;

fn totals(v: &Totals) -> (u64, f64, Option<i64>) {
    (v.count, v.amount, v.quantity)
}

#[tokio::test]
async fn values_ranges_and_listings() {
    let t = TestApp::new().await.unwrap();
    let a = t.mutation::<add>(("ann".to_string(), "2026-10-01".to_string(), 10.0, Some(2))).await.unwrap();
    t.mutation::<add>(("ann".to_string(), "2026-10-02".to_string(), 5.5, Some(1))).await.unwrap();
    t.mutation::<add>(("bob".to_string(), "2026-10-01".to_string(), 1.0, None)).await.unwrap();
    t.mutation::<add>(("ann".to_string(), "2026-10-05".to_string(), 2.0, Some(4))).await.unwrap();

    assert_eq!(totals(&t.query::<customer>(("ann".to_string(),)).await.unwrap()), (3, 17.5, Some(7)));
    assert_eq!(
        totals(&t.query::<customer>(("bob".to_string(),)).await.unwrap()),
        (1, 1.0, Some(0)),
        "a null adds nothing"
    );
    assert_eq!(
        totals(&t.query::<customer>(("zed".to_string(),)).await.unwrap()),
        (0, 0.0, Some(0)),
        "no documents: zero"
    );
    assert_eq!(totals(&t.query::<total>(()).await.unwrap()), (4, 18.5, None));
    let days = |from: &str, to: &str| t.query::<fns::days>(("ann".to_string(), from.to_string(), to.to_string()));
    assert_eq!(totals(&days("2026-10-01", "2026-10-02").await.unwrap()), (2, 15.5, None));
    assert_eq!(totals(&days("2026-10-03", "2026-10-31").await.unwrap()), (1, 2.0, None));

    let groups = t.query::<fns::groups>((false, false, 10u32)).await.unwrap();
    assert_eq!(groups, vec![vec![json!("ann"), json!(3), json!(17.5)], vec![json!("bob"), json!(1), json!(1.0)]]);
    let top = t.query::<fns::groups>((true, true, 2u32)).await.unwrap();
    assert_eq!(
        top,
        vec![
            vec![json!("bob"), json!("2026-10-01"), json!(1), json!(1.0)],
            vec![json!("ann"), json!("2026-10-05"), json!(1), json!(2.0)]
        ]
    );

    // Moving a document to another group, changing a summed field, deleting it.
    t.mutation::<patch>((a, json!({ "customer": "bob" }))).await.unwrap();
    assert_eq!(totals(&t.query::<customer>(("bob".to_string(),)).await.unwrap()), (2, 11.0, Some(2)));
    t.mutation::<patch>((a, json!({ "amount": 20.0 }))).await.unwrap();
    assert_eq!(totals(&t.query::<customer>(("bob".to_string(),)).await.unwrap()), (2, 21.0, Some(2)));
    t.mutation::<remove>((a,)).await.unwrap();
    assert_eq!(totals(&t.query::<customer>(("bob".to_string(),)).await.unwrap()), (1, 1.0, Some(0)));
    assert_eq!(totals(&t.query::<total>(()).await.unwrap()), (3, 8.5, None));

    // The dashboard's table summary lists them.
    let admin = rxferrix::Identity::admin(
        t.app().auth().create_admin(t.app(), "a@example.com", "admin password".into()).await.unwrap(),
        None,
    );
    let tables = t.app().run_query("_system/dashboard:tables", json!({}), &admin).await.result.unwrap();
    let sales = tables.as_array().unwrap().iter().find(|t| t["name"] == "agg_orders").unwrap();
    assert_eq!(
        sales["aggregates"][1],
        json!({ "name": "by_customer_day", "groupFields": ["customer", "day"], "sumFields": ["amount"] })
    );

    let e = t.query::<bad_get>(()).await.unwrap_err();
    assert!(e.to_string().contains("get needs an `eq` on every group field"), "{e}");
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

/// Random inserts, patches (group and amounts) and deletes: every group matches the documents, exactly for integers.
#[tokio::test]
async fn the_aggregates_match_the_documents() {
    let t = TestApp::new().await.unwrap();
    let mut live: BTreeMap<String, (String, String, f64, Option<i64>)> = BTreeMap::new();
    let mut seed = 11u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for _ in 0..400 {
        let r = next();
        let ids: Vec<String> = live.keys().cloned().collect();
        let customer = format!("c{}", r % 5);
        let day = format!("d{}", (r / 7) % 4);
        let amount = ((r / 11) % 1000) as f64 / 4.0;
        let quantity = (r % 3 != 0).then_some(((r / 13) % 50) as i64);
        match r % 10 {
            0..=4 => {
                let id = t.mutation::<add>((customer.clone(), day.clone(), amount, quantity)).await.unwrap();
                live.insert(id.to_string(), (customer, day, amount, quantity));
            }
            5..=7 if !ids.is_empty() => {
                let id = &ids[(r as usize / 3) % ids.len()];
                let fields = json!({ "customer": customer, "amount": amount, "quantity": quantity });
                t.mutation::<patch>((id.parse::<rxferrix::Id<Sale>>().unwrap(), fields)).await.unwrap();
                let doc = live.get_mut(id).unwrap();
                (doc.0, doc.2, doc.3) = (customer, amount, quantity);
            }
            _ if !ids.is_empty() => {
                let id = &ids[(r as usize / 3) % ids.len()];
                t.mutation::<remove>((id.parse::<rxferrix::Id<Sale>>().unwrap(),)).await.unwrap();
                live.remove(id);
            }
            _ => {}
        }
    }
    let mut by_customer: BTreeMap<String, (u64, f64, i64)> = BTreeMap::new();
    let mut by_day: BTreeMap<(String, String), (u64, f64)> = BTreeMap::new();
    for (c, d, amount, qty) in live.values() {
        let e = by_customer.entry(c.clone()).or_default();
        e.0 += 1;
        e.1 += amount;
        e.2 += qty.unwrap_or(0);
        let e = by_day.entry((c.clone(), d.clone())).or_default();
        e.0 += 1;
        e.1 += amount;
    }
    for (c, (n, amount, qty)) in &by_customer {
        let got = t.query::<customer>((c.clone(),)).await.unwrap();
        assert_eq!((got.count, got.quantity), (*n, Some(*qty)), "{c}");
        assert!((got.amount - amount).abs() < 1e-6, "{c}: {} vs {amount}", got.amount);
    }
    let daily = t.query::<fns::groups>((true, false, 1000u32)).await.unwrap();
    assert_eq!(daily.len(), by_day.len(), "empty groups are gone");
    for (row, ((c, d), (n, amount))) in daily.iter().zip(&by_day) {
        assert_eq!(
            (row[0].as_str().unwrap(), row[1].as_str().unwrap(), row[2].as_u64().unwrap()),
            (c.as_str(), d.as_str(), *n)
        );
        assert!((row[3].as_f64().unwrap() - amount).abs() < 1e-6);
    }
    let total = t.query::<total>(()).await.unwrap();
    assert_eq!(total.count, live.len() as u64);
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

/// A write touches only the groups it changes: a query reading another group isn't invalidated, and a write that
/// changes nothing aggregated (another field) invalidates no aggregate read.
#[tokio::test]
async fn reactivity_is_narrowed_to_the_changed_groups() {
    let t = TestApp::new().await.unwrap();
    let app = t.app();
    let ann = t.mutation::<add>(("ann".to_string(), "d1".to_string(), 1.0, Some(1))).await.unwrap();
    t.mutation::<add>(("bob".to_string(), "d1".to_string(), 1.0, Some(1))).await.unwrap();
    let read = t.query_run::<customer>(("ann".to_string(),)).await.unwrap();
    read.result.unwrap();
    let commit = |ts: u64| app.db().write_log().entries_between(ts - 1, ts).unwrap().pop().unwrap();
    let invalidates = |ts: u64| read.read_set.overlaps(&commit(ts));

    let (_, ts) = t.mutation_ts::<add>(("bob".to_string(), "d2".to_string(), 3.0, None)).await.unwrap();
    assert!(!invalidates(ts), "a write to bob's group");
    let (_, ts) = t.mutation_ts::<patch>((ann, json!({ "note": "unrelated" }))).await.unwrap();
    assert!(!invalidates(ts), "no aggregated value changed");
    let (_, ts) = t.mutation_ts::<patch>((ann, json!({ "day": "d9" }))).await.unwrap();
    assert!(!invalidates(ts), "only by_customer_day changed");
    let (_, ts) = t.mutation_ts::<patch>((ann, json!({ "amount": 2.0 }))).await.unwrap();
    assert!(invalidates(ts), "ann's sum changed");
    let (_, ts) = t.mutation_ts::<add>(("ann".to_string(), "d1".to_string(), 1.0, None)).await.unwrap();
    assert!(invalidates(ts), "ann's count changed");
}

#[tokio::test]
async fn rollbacks_and_reading_your_own_writes() {
    let t = TestApp::new().await.unwrap();
    t.mutation::<add>(("ann".to_string(), "d".to_string(), 1.0, Some(1))).await.unwrap();
    let e = t.mutation::<add_then_fail>(("ann".to_string(),)).await.unwrap_err();
    assert!(e.to_string().contains("failing on purpose"));
    assert_eq!(
        totals(&t.query::<customer>(("ann".to_string(),)).await.unwrap()),
        (1, 1.0, Some(1)),
        "rolled back with the insert"
    );
    assert_eq!(t.mutation::<add_and_count>(("ann".to_string(),)).await.unwrap(), 2, "the mutation's own insert counts");
    assert!(t.app().db().verify_indexes().await.unwrap().is_empty());
}

/// An aggregate added to a table with documents is built from them; a changed one is rebuilt; a removed one deleted;
/// tampering is caught by verification.
#[tokio::test]
async fn backfill_redefinition_and_verification() {
    use crate::common::items;
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions::default()).await.unwrap();
        for v in 0..30 {
            app.mutation::<items::insert>((v % 4, v, format!("n{v}"))).await.unwrap();
        }
    }
    let with = |aggregates: Vec<AggregateDef>| TableDef { aggregates, ..<items::Item as rxferrix::Table>::table_def() };
    let stored = || -> Vec<(i64, String)> {
        let conn = rusqlite::Connection::open(dir.path().join("data.db")).unwrap();
        let mut stmt = conn.prepare("SELECT count, sums FROM aggregate_values ORDER BY key").unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
    };
    // Added: built from the 30 documents, 4 groups.
    {
        let def = with(vec![AggregateDef { name: "by_group", group_fields: vec!["group"], sum_fields: vec!["score"] }]);
        let db = Database::open(dir.path(), std::slice::from_ref(&def), 2).unwrap();
        assert!(db.verify_indexes().await.unwrap().is_empty());
    }
    let rows = stored();
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), vec![8, 8, 7, 7]);
    let score_sum: i64 = rows.iter().map(|r| serde_json::from_str::<Vec<i64>>(&r.1).unwrap()[0]).sum();
    assert_eq!(score_sum, (0..30).sum::<i64>(), "integer sums are exact");
    // Changed (sum field removed): rebuilt.
    {
        let def = with(vec![AggregateDef { name: "by_group", group_fields: vec!["group"], sum_fields: vec![] }]);
        let db = Database::open(dir.path(), std::slice::from_ref(&def), 2).unwrap();
        assert!(db.verify_indexes().await.unwrap().is_empty());
    }
    assert!(stored().iter().all(|r| r.1 == "[]"));
    // Tampered with: verification says so.
    rusqlite::Connection::open(dir.path().join("data.db"))
        .unwrap()
        .execute("UPDATE aggregate_values SET count = count + 1", [])
        .unwrap();
    {
        let def = with(vec![AggregateDef { name: "by_group", group_fields: vec!["group"], sum_fields: vec![] }]);
        let db = Database::open(dir.path(), std::slice::from_ref(&def), 2).unwrap();
        let problems = db.verify_indexes().await.unwrap();
        assert!(problems.iter().any(|p| p.contains("aggregate `items.by_group`")), "{problems:?}");
    }
    // Removed: its rows go.
    {
        let def = with(vec![]);
        Database::open(dir.path(), std::slice::from_ref(&def), 2).unwrap();
    }
    assert!(stored().is_empty());
    let _: Value = json!(null);
}
