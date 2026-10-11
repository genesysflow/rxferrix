//! The analytics copy (`ferrix::olap`, feature `olap`): typed columns, batches of inserts, patches and deletes, the
//! copy matching the live data, restarts, rebuilds (restore, journal gap, shape change, falling behind), journal
//! trimming, and the sandbox around `ctx.olap`.
#![cfg(feature = "olap")]

use std::collections::BTreeMap;
use std::time::Duration;

use ferrix::olap::OlapOptions;
use ferrix::testing::TestApp;
use ferrix::{AppOptions, Error, UserError};
use serde_json::{Value, json};

pub mod fns {
    use ferrix::prelude::*;
    use serde_json::{Value, json};

    #[object]
    #[derive(Clone, Debug, PartialEq)]
    /// Not in alphabetical order: the struct column keeps declaration order.
    pub struct Address {
        pub zip: Option<String>,
        pub city: String,
    }

    #[object]
    #[derive(Clone, Debug, PartialEq)]
    pub enum Tier {
        Free,
        Pro,
    }

    #[table(name = "olap_orders")]
    #[derive(PartialEq)]
    pub struct Order {
        pub customer: String,
        pub amount: f64,
        pub quantity: i64,
        pub paid: bool,
        pub note: Option<String>,
        pub tags: Vec<String>,
        pub address: Address,
        pub tier: Tier,
        pub receipt: Option<Bytes>,
        pub extra: Value,
    }

    pub fn order(customer: &str, amount: f64, quantity: i64) -> Order {
        Order {
            customer: customer.into(),
            amount,
            quantity,
            paid: quantity % 2 == 0,
            note: (quantity % 3 == 0).then(|| format!("note {quantity}")),
            tags: vec!["a".into(), format!("q{quantity}")],
            address: Address { zip: None, city: format!("city-{customer}") },
            tier: if amount > 50.0 { Tier::Pro } else { Tier::Free },
            receipt: Some(Bytes::new(vec![1, 2, 3])),
            extra: json!({ "k": quantity }),
        }
    }

    #[internal_mutation]
    pub async fn add(ctx: &MutationCtx, customer: String, amount: f64, quantity: i64) -> Result<Id<Order>> {
        ctx.db.insert(order(&customer, amount, quantity)).await
    }

    #[internal_mutation]
    pub async fn set_amount(ctx: &MutationCtx, id: Id<Order>, amount: f64) -> Result<()> {
        ctx.db.patch(id, json!({ "amount": amount })).await
    }

    #[internal_mutation]
    pub async fn remove(ctx: &MutationCtx, id: Id<Order>) -> Result<()> {
        ctx.db.delete(id).await
    }

    #[internal_action]
    pub async fn sql(ctx: &ActionCtx, sql: String, params: Vec<Value>) -> Result<Value> {
        let params = params
            .into_iter()
            .map(|p| match p {
                Value::Number(n) if n.is_i64() => n.as_i64().unwrap().into(),
                Value::Number(n) => n.as_f64().unwrap().into(),
                Value::String(s) => s.into(),
                Value::Bool(b) => b.into(),
                _ => ferrix::olap::OlapValue::Null,
            })
            .collect::<Vec<ferrix::olap::OlapValue>>();
        Ok(serde_json::to_value(ctx.olap.query(&sql, params).await?)?)
    }

    /// Read-your-writes: insert, then query at the mutation's commit.
    #[internal_action]
    pub async fn add_then_count(ctx: &ActionCtx, customer: String) -> Result<i64> {
        let id: Id<Order> = ctx.run_mutation::<add>((customer.clone(), 1.0, 1)).await?;
        let _ = id;
        #[object]
        struct N {
            n: i64,
        }
        let rows = ctx
            .olap
            .latest()
            .query_as::<N>("SELECT count(*) AS n FROM olap_orders WHERE customer = ?", (customer,))
            .await?;
        Ok(rows[0].n)
    }
}

use fns::*;

fn olap(background: bool) -> AppOptions {
    AppOptions {
        dev: true,
        olap: OlapOptions { enabled: true, background, ..Default::default() },
        ..Default::default()
    }
}

async fn sql(t: &TestApp, sql: &str) -> Result<Value, Error> {
    t.action::<fns::sql>((sql.to_string(), Vec::<Value>::new())).await
}

async fn rows(t: &TestApp, q: &str) -> Vec<Value> {
    sql(t, q).await.unwrap()["rows"].as_array().unwrap().clone()
}

fn code(e: &Error) -> String {
    e.downcast_ref::<UserError>().map(|u| u.code.clone()).unwrap_or_else(|| format!("not a user error: {e:#}"))
}

#[tokio::test]
async fn typed_columns_and_values() {
    let t = TestApp::with_options(olap(false)).await.unwrap();
    t.mutation::<add>(("ann".to_string(), 75.5, 3)).await.unwrap();
    t.olap_sync().await.unwrap();
    let out = sql(&t, "SELECT * FROM olap_orders").await.unwrap();
    let columns: Vec<(String, String)> = out["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["name"].as_str().unwrap().into(), c["typeName"].as_str().unwrap().into()))
        .collect();
    let names: Vec<&str> = columns.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        ["_id", "_creationTime", "_ts", "customer", "amount", "quantity", "paid", "note", "tags", "address", "tier", "receipt", "extra"]
    );
    let types: BTreeMap<&str, &str> = columns.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect();
    assert_eq!(types["amount"], "DOUBLE");
    assert_eq!(types["quantity"], "BIGINT");
    assert_eq!(types["paid"], "BOOLEAN");
    assert_eq!(types["tags"], "LIST");
    assert_eq!(types["address"], "STRUCT");
    assert_eq!(types["receipt"], "BLOB");
    assert_eq!(types["extra"], "JSON");
    let row = &out["rows"][0];
    assert_eq!(row["customer"], "ann");
    assert_eq!(row["amount"], 75.5);
    assert_eq!(row["quantity"], 3);
    assert_eq!(row["paid"], false);
    assert_eq!(row["note"], "note 3");
    assert_eq!(row["tags"], json!(["a", "q3"]));
    assert_eq!(row["address"], json!({ "zip": null, "city": "city-ann" }));
    let address_type = columns.iter().find(|(n, _)| n == "address").unwrap();
    assert_eq!(address_type.1, "STRUCT");
    assert_eq!(row["tier"], "pro");
    assert_eq!(row["receipt"], json!({ "$bytes": "AQID" }));
    assert_eq!(row["extra"], json!({ "k": 3 }), "JSON columns come back parsed");
    assert!(row["_id"].as_str().unwrap().len() > 10);
    assert_eq!(out["asOfTs"], row["_ts"]);
    // Struct fields and list functions work in SQL.
    let r = rows(&t, "SELECT address.city AS city, len(tags) AS n, tags[2] AS second FROM olap_orders").await;
    assert_eq!(r[0], json!({ "city": "city-ann", "n": 2, "second": "q3" }));
}

/// After random inserts, patches and deletes (applied in many batches), every aggregate matches the live data.
#[tokio::test]
async fn the_copy_matches_the_data() {
    let t = TestApp::with_options(olap(false)).await.unwrap();
    let mut live: BTreeMap<String, (String, f64, i64)> = BTreeMap::new();
    let mut seed = 7u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..30 {
        for _ in 0..20 {
            let r = next();
            let ids: Vec<String> = live.keys().cloned().collect();
            match r % 10 {
                0..=5 => {
                    let customer = format!("c{}", r % 7);
                    let (amount, qty) = ((r % 1000) as f64 / 10.0, (r % 13) as i64);
                    let id = t.mutation::<add>((customer.clone(), amount, qty)).await.unwrap();
                    live.insert(id.to_string(), (customer, amount, qty));
                }
                6..=7 if !ids.is_empty() => {
                    let id = &ids[(r as usize / 10) % ids.len()];
                    let amount = (r % 500) as f64;
                    t.mutation::<set_amount>((id.parse::<ferrix::Id<Order>>().unwrap(), amount)).await.unwrap();
                    live.get_mut(id).unwrap().1 = amount;
                }
                _ if !ids.is_empty() => {
                    let id = &ids[(r as usize / 10) % ids.len()];
                    t.mutation::<remove>((id.parse::<ferrix::Id<Order>>().unwrap(),)).await.unwrap();
                    live.remove(id);
                }
                _ => {}
            }
        }
        if round % 3 == 0 {
            t.olap_sync().await.unwrap();
        }
    }
    t.olap_sync().await.unwrap();
    let mut expected: BTreeMap<String, (i64, f64, i64)> = BTreeMap::new();
    for (customer, amount, qty) in live.values() {
        let e = expected.entry(customer.clone()).or_default();
        e.0 += 1;
        e.1 += amount;
        e.2 += qty;
    }
    let got = rows(&t, "SELECT customer, count(*) AS n, sum(amount) AS total, sum(quantity) AS qty FROM olap_orders GROUP BY customer ORDER BY customer").await;
    let got: BTreeMap<String, (i64, f64, i64)> = got
        .iter()
        .map(|r| {
            (r["customer"].as_str().unwrap().into(), (r["n"].as_i64().unwrap(), r["total"].as_f64().unwrap(), r["qty"].as_i64().unwrap()))
        })
        .collect();
    assert_eq!(got.len(), expected.len());
    for (c, (n, total, qty)) in &expected {
        let g = got[c];
        assert_eq!((g.0, g.2), (*n, *qty), "{c}");
        assert!((g.1 - total).abs() < 1e-6, "{c}: {} vs {total}", g.1);
    }
    let ids = rows(&t, "SELECT _id FROM olap_orders ORDER BY _id").await;
    let ids: Vec<&str> = ids.iter().map(|r| r["_id"].as_str().unwrap()).collect();
    assert_eq!(ids, live.keys().map(String::as_str).collect::<Vec<_>>(), "exactly the live documents");
    // The journal was released: nothing waits in it.
    let status = t.app().olap().unwrap().status(t.app()).await;
    assert_eq!((status.state.as_str(), status.applied_ts, status.latest_ts), ("ready", t.app().db().write_log().latest(), t.app().db().write_log().latest()));
    assert_eq!(t.app().db().journal().pending_bytes(), 0, "applied rows are trimmed when replication is off");
}

#[tokio::test]
async fn restarts_resume_and_restores_rebuild() {
    let t = TestApp::with_options(olap(false)).await.unwrap();
    for i in 0..5 {
        t.mutation::<add>((format!("c{i}"), 10.0, i)).await.unwrap();
    }
    t.olap_sync().await.unwrap();
    assert_eq!(t.app().olap().unwrap().status(t.app()).await.rebuilds, 1, "the first build");

    // A restart continues from where the copy was.
    let t = t.restart().await.unwrap();
    t.mutation::<add>(("late".to_string(), 1.0, 1)).await.unwrap();
    t.olap_sync().await.unwrap();
    assert_eq!(t.app().olap().unwrap().status(t.app()).await.rebuilds, 0, "no rebuild after a restart");
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 6);

    // Writes made while the copy is off leave a gap in the journal: the next start rebuilds.
    let t = t.restart_with(AppOptions { dev: true, ..Default::default() }).await.unwrap();
    t.mutation::<add>(("offline".to_string(), 1.0, 1)).await.unwrap();
    let t = t.restart_with(olap(false)).await.unwrap();
    t.olap_sync().await.unwrap();
    let status = t.app().olap().unwrap().status(t.app()).await;
    assert_eq!(status.rebuilds, 1, "rebuilt: {status:?}");
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 7);

    // A restored database (its journal and copy id scrubbed) rebuilds too.
    let dir = t.dir().to_path_buf();
    let t = t.restart_with(AppOptions { dev: true, ..Default::default() }).await.unwrap();
    ferrix::db::journal::scrub(&rusqlite::Connection::open(dir.join("data.db")).unwrap()).unwrap();
    let t = t.restart_with(olap(false)).await.unwrap();
    t.olap_sync().await.unwrap();
    assert_eq!(t.app().olap().unwrap().status(t.app()).await.rebuilds, 1);
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 7);
}

#[tokio::test]
async fn a_changed_shape_rebuilds_only_that_table() {
    let t = TestApp::with_options(olap(false)).await.unwrap();
    t.mutation::<add>(("ann".to_string(), 1.0, 1)).await.unwrap();
    t.olap_sync().await.unwrap();
    let dir = t.dir().to_path_buf();
    let t = t.restart_with(AppOptions { dev: true, ..Default::default() }).await.unwrap();
    // As if the table's type had changed in code since the copy was made.
    {
        let duck = duckdb::Connection::open(dir.join("olap.duckdb")).unwrap();
        duck.execute("UPDATE _ferrix_tables SET fingerprint = 'old' WHERE name = 'olap_orders'", []).unwrap();
    }
    let t = t.restart_with(olap(false)).await.unwrap();
    t.mutation::<add>(("bob".to_string(), 2.0, 2)).await.unwrap();
    t.olap_sync().await.unwrap();
    let status = t.app().olap().unwrap().status(t.app()).await;
    assert_eq!(status.rebuilds, 1, "only olap_orders: {status:?}");
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 2);
    assert_eq!(status.state, "ready");
}

#[tokio::test]
async fn falling_behind_rebuilds_instead_of_holding_the_journal() {
    let mut options = olap(false);
    options.olap.max_lag_bytes = 2_000;
    let t = TestApp::with_options(options).await.unwrap();
    t.olap_sync().await.unwrap();
    for i in 0..40 {
        t.mutation::<add>((format!("c{i}"), 1.0, i)).await.unwrap();
    }
    assert!(t.app().db().journal().pending_bytes() > 2_000);
    t.olap_sync().await.unwrap();
    let status = t.app().olap().unwrap().status(t.app()).await;
    assert_eq!(status.rebuilds, 2, "the first build, then one for falling behind: {status:?}");
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 40);
}

#[tokio::test]
async fn system_tables_without_secrets() {
    let t = TestApp::with_options(olap(false)).await.unwrap();
    t.create_user("ann@example.com", Some("password123")).await.unwrap();
    t.olap_sync().await.unwrap();
    let users = rows(&t, "SELECT email FROM _users").await;
    assert_eq!(users[0]["email"], "ann@example.com");
    for secret in ["_credentials", "_sessions", "_settings", "_admins", "_verification_codes", "_audit_log"] {
        let e = sql(&t, &format!("SELECT * FROM {secret}")).await.unwrap_err();
        assert_eq!(code(&e), "olap_query_failed", "{secret} must not be in the copy");
    }
    let tables = rows(&t, "SELECT table_name FROM information_schema.tables ORDER BY 1").await;
    let names: Vec<&str> = tables.iter().map(|r| r["table_name"].as_str().unwrap()).collect();
    assert!(!names.iter().any(|n| n.contains("credential") || n.contains("session")), "{names:?}");
}

#[tokio::test]
async fn the_sandbox() {
    let mut options = olap(false);
    options.olap.query_timeout = Duration::from_millis(300);
    options.olap.max_result_rows = 1000;
    let t = TestApp::with_options(options).await.unwrap();
    t.mutation::<add>(("ann".to_string(), 1.0, 1)).await.unwrap();
    t.olap_sync().await.unwrap();
    let secret = t.dir().join("secret.csv");
    std::fs::write(&secret, "token\nhunter2\n").unwrap();
    let path = secret.display().to_string();
    let rejected = [
        "DELETE FROM olap_orders".to_string(),
        "INSERT INTO olap_orders (_id) VALUES ('x')".into(),
        "UPDATE olap_orders SET amount = 0".into(),
        "DROP TABLE olap_orders".into(),
        "CREATE TABLE x (a INT)".into(),
        "SELECT 1; DELETE FROM olap_orders".into(),
        "SET enable_external_access = true".into(),
        "PRAGMA database_list".into(),
        format!("ATTACH '{path}.db' AS other"),
        format!("COPY olap_orders TO '{path}.out'"),
        "INSTALL httpfs".into(),
        "CHECKPOINT".into(),
        "SELECT * FROM query('DELETE FROM olap_orders')".into(),
        format!("SELECT * FROM read_csv('{path}')"),
        format!("SELECT * FROM read_text('{path}')"),
        format!("SELECT * FROM glob('{}/*')", t.dir().display()),
        "SELECT * FROM read_csv('https://example.com/x.csv')".into(),
        "SELECT getenv('HOME')".into(),
    ];
    for q in &rejected {
        let e = sql(&t, q).await.expect_err(q);
        assert_eq!(code(&e), "olap_query_failed", "{q}: {e:#}");
    }
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 1, "nothing was written");
    assert!(!t.dir().join("secret.csv.out").exists() && !t.dir().join("secret.csv.db").exists());

    // Allowed: CTEs, parameters.
    let r = t
        .action::<fns::sql>(("WITH x AS (SELECT ? AS a) SELECT a + 1 AS b FROM x".to_string(), vec![json!(41)]))
        .await
        .unwrap();
    assert_eq!(r["rows"][0]["b"], 42);
    // Limits.
    let e = sql(&t, "SELECT * FROM range(5000)").await.unwrap_err();
    assert_eq!(code(&e), "olap_query_failed");
    assert!(e.to_string().contains("LIMIT"), "{e}");
    let started = std::time::Instant::now();
    let e = sql(&t, "SELECT count(*) FROM range(10000000) a, range(10000000) b").await.unwrap_err();
    assert_eq!(code(&e), "olap_timeout", "{e:#}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // Still usable after the interrupt.
    assert_eq!(rows(&t, "SELECT 1 AS one").await[0]["one"], 1);
}

#[tokio::test]
async fn read_your_writes_in_the_background() {
    let t = TestApp::with_options(olap(true)).await.unwrap();
    for i in 0..3 {
        assert_eq!(t.action::<add_then_count>(("zed".to_string(),)).await.unwrap(), i + 1);
    }
}

#[tokio::test]
async fn unavailable_without_olap() {
    let t = TestApp::new().await.unwrap();
    let e = sql(&t, "SELECT 1").await.unwrap_err();
    assert_eq!(code(&e), "olap_unavailable");
    assert!(!t.dir().join("olap.duckdb").exists());
}

/// Replication and the copy read one journal: a row goes once both have released it.
#[tokio::test]
async fn replication_and_the_copy_share_the_journal() {
    use ferrix::ops::OpsOptions;
    use ferrix::ops::backup::BackupTarget;
    use ferrix::ops::replicate::ReplicationOptions;

    let target_dir = tempfile::tempdir().unwrap();
    let target = BackupTarget::parse(target_dir.path().to_str().unwrap(), target_dir.path()).unwrap();
    let mut replication = ReplicationOptions::with_target(target);
    replication.background = false;
    let mut options = olap(false);
    options.ops = OpsOptions { replication: Some(replication), ..Default::default() };
    let t = TestApp::with_options(options).await.unwrap();
    let journal = |t: &TestApp| {
        let conn = rusqlite::Connection::open(t.dir().join("data.db")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM _journal", [], |r| r.get::<_, i64>(0)).unwrap()
    };
    let rep = t.app().replication().unwrap().clone();
    for i in 0..5 {
        t.mutation::<add>((format!("c{i}"), 1.0, i)).await.unwrap();
    }
    t.olap_sync().await.unwrap();
    assert!(journal(&t) >= 5, "applied, but not shipped yet: kept");
    rep.sync_now(t.app()).await.unwrap();
    assert_eq!(journal(&t), 0, "applied and shipped: gone");

    for i in 0..3 {
        t.mutation::<add>((format!("d{i}"), 1.0, i)).await.unwrap();
    }
    rep.sync_now(t.app()).await.unwrap();
    assert_eq!(journal(&t), 3, "shipped, but not applied yet: kept");
    t.olap_sync().await.unwrap();
    assert_eq!(journal(&t), 0);
    assert_eq!(rows(&t, "SELECT count(*) AS n FROM olap_orders").await[0]["n"], 8);
}
