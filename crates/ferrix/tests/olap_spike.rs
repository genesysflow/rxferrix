//! Phase 11 spike (throwaway, branch spike/olap): DuckDB throughput and the sandbox, measured directly.
//!
//!   cargo test --release -p ferrix --features olap --test olap_spike -- --nocapture --test-threads 1
#![cfg(feature = "olap")]

use std::time::{Duration, Instant};

use duckdb::{Config, Connection, params};

const ROWS: usize = 1_000_000;
const CHANNELS: usize = 50;

fn open(dir: &std::path::Path) -> Connection {
    let conn = Connection::open_with_flags(dir.join("olap.duckdb"), Config::default()).unwrap();
    conn.execute_batch(
        "CREATE TABLE messages (_id VARCHAR PRIMARY KEY, _creationTime DOUBLE, _ts BIGINT, channel VARCHAR,
         author VARCHAR, amount DOUBLE, body VARCHAR)",
    )
    .unwrap();
    conn
}

fn row(i: usize, ts: i64) -> (String, f64, i64, String, String, f64, String) {
    (
        format!("doc{i:010}"),
        1_760_000_000_000.0 + i as f64 * 1000.0,
        ts,
        format!("channel{}", i % CHANNELS),
        format!("user{}", i % 997),
        (i % 1000) as f64 / 10.0,
        format!("message number {i} with some text to make it realistic"),
    )
}

fn ingest(conn: &Connection) -> Duration {
    let started = Instant::now();
    let mut appender = conn.appender("messages").unwrap();
    for i in 0..ROWS {
        let r = row(i, 1);
        appender.append_row(params![r.0, r.1, r.2, r.3, r.4, r.5, r.6]).unwrap();
    }
    appender.flush().unwrap();
    started.elapsed()
}

#[test]
fn throughput() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let took = ingest(&conn);
    println!("ingest {ROWS} rows with the Appender: {took:?} ({:.0} rows/s)", ROWS as f64 / took.as_secs_f64());

    // A batch of 1k changes: delete the written ids, append their new states, in one transaction.
    let mut batches = Vec::new();
    for b in 0..20usize {
        let started = Instant::now();
        conn.execute_batch("BEGIN").unwrap();
        let ids: Vec<String> = (0..1000).map(|k| format!("'doc{:010}'", (b * 7919 + k * 997) % ROWS)).collect();
        conn.execute(&format!("DELETE FROM messages WHERE _id IN ({})", ids.join(",")), []).unwrap();
        {
            let mut appender = conn.appender("messages").unwrap();
            for k in 0..1000 {
                let r = row((b * 7919 + k * 997) % ROWS, 2 + b as i64);
                appender.append_row(params![r.0, r.1, r.2, r.3, r.4, r.5 + 1.0, r.6]).unwrap();
            }
            appender.flush().unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
        batches.push(started.elapsed());
    }
    batches.sort();
    println!("apply 1k changes (delete + append, one transaction): p50 {:?}, max {:?}", batches[10], batches[19]);
    let n: i64 = conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0)).unwrap();
    assert_eq!(n as usize, ROWS, "deletes and appends of the same ids keep the count");

    for (label, sql) in [
        ("group by channel: count, sum", "SELECT channel, count(*), sum(amount) FROM messages GROUP BY channel"),
        (
            "group by day",
            "SELECT date_trunc('day', to_timestamp(_creationTime / 1000)) AS day, count(*) FROM messages GROUP BY day",
        ),
        ("distinct authors per channel", "SELECT channel, count(DISTINCT author) FROM messages GROUP BY channel"),
        ("top 10 authors by amount", "SELECT author, sum(amount) s FROM messages GROUP BY author ORDER BY s DESC LIMIT 10"),
    ] {
        let started = Instant::now();
        let mut stmt = conn.prepare(sql).unwrap();
        let rows = stmt.query_map([], |_| Ok(())).unwrap().count();
        println!("{label} over {ROWS} rows: {:?} ({rows} groups)", started.elapsed());
    }
    let size = std::fs::metadata(dir.path().join("olap.duckdb")).unwrap().len();
    println!("file size after checkpoint-free writes: {} MiB", size >> 20);
}

/// A reader on another connection sees whole transactions while a batch applies.
#[test]
fn readers_see_whole_batches() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    conn.execute_batch("CREATE TABLE meta (applied BIGINT); INSERT INTO meta VALUES (0);").unwrap();
    let reader = conn.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        for b in 1..=200i64 {
            conn.execute_batch("BEGIN").unwrap();
            {
                let mut appender = conn.appender("messages").unwrap();
                for k in 0..100 {
                    let r = row((b * 100 + k) as usize, b);
                    appender.append_row(params![r.0, r.1, r.2, r.3, r.4, r.5, r.6]).unwrap();
                }
                appender.flush().unwrap();
            }
            conn.execute("UPDATE meta SET applied = ?", [b]).unwrap();
            conn.execute_batch("COMMIT").unwrap();
        }
    });
    let mut checks = 0;
    while !writer.is_finished() {
        // One snapshot for both reads.
        reader.execute_batch("BEGIN").unwrap();
        let applied: i64 = reader.query_row("SELECT applied FROM meta", [], |r| r.get(0)).unwrap();
        let rows: i64 = reader.query_row("SELECT count(*) FROM messages", [], |r| r.get(0)).unwrap();
        reader.execute_batch("COMMIT").unwrap();
        assert_eq!(rows, applied * 100, "a reader saw part of a batch");
        checks += 1;
    }
    writer.join().unwrap();
    println!("{checks} consistent snapshots while 200 batches applied");
}

/// What a sandboxed query connection can and can't do.
#[test]
fn sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let conn = open(dir.path());
    let secret = dir.path().join("secret.csv");
    std::fs::write(&secret, "token\nhunter2\n").unwrap();
    let read_secret = format!("SELECT * FROM read_csv('{}')", secret.display());
    assert!(conn.query_row(&read_secret, [], |_| Ok(())).is_ok(), "control: readable before the lockdown");

    conn.execute_batch(
        "SET enable_external_access = false; SET autoload_known_extensions = false; SET autoinstall_known_extensions = false;
         SET memory_limit = '256MB'; SET threads = 2; SET lock_configuration = true;",
    )
    .unwrap();
    let blocked = |sql: &str| {
        let r = conn.execute_batch(sql);
        println!("  {:<60} -> {}", sql.chars().take(60).collect::<String>(), match &r {
            Ok(()) => "ALLOWED".to_string(),
            Err(e) => format!("blocked: {}", e.to_string().lines().next().unwrap_or_default()),
        });
        r.is_err()
    };
    println!("after the lockdown:");
    assert!(blocked(&read_secret), "files must be unreadable");
    assert!(blocked("SELECT * FROM read_csv('https://example.com/x.csv')"), "no HTTP");
    assert!(blocked("SET enable_external_access = true"), "configuration locked");
    assert!(blocked(&format!("ATTACH '{}' AS other", dir.path().join("other.duckdb").display())), "no ATTACH");
    assert!(blocked(&format!("COPY messages TO '{}'", dir.path().join("out.csv").display())), "no COPY TO");
    assert!(blocked("INSTALL httpfs"), "no extensions");
    // Writes are not blocked by the settings: the SELECT-only check has to do it.
    let write_allowed = !blocked("INSERT INTO messages (_id) VALUES ('x')");
    println!("plain INSERT allowed by the settings alone: {write_allowed}");

    // Candidate SELECT-only checks.
    println!("prepare() with two statements:");
    let two = conn.prepare("SELECT 1; DELETE FROM messages");
    println!("  -> {}", if two.is_ok() { "ALLOWED".into() } else { format!("rejected: {}", two.err().unwrap()) });
    println!("json_serialize_sql():");
    for sql in [
        "SELECT channel, count(*) FROM messages GROUP BY channel",
        "WITH t AS (SELECT 1 AS x) SELECT * FROM t",
        "DELETE FROM messages",
        "INSERT INTO messages (_id) VALUES ('y')",
        "SELECT 1; DELETE FROM messages",
        "PRAGMA database_list",
    ] {
        let out: String = conn.query_row("SELECT json_serialize_sql(?::VARCHAR)", [sql], |r| r.get(0)).unwrap();
        let error = out.contains("\"error\":true");
        let statements = out.matches("\"node\"").count().max(out.matches("\"from_table\"").count());
        println!("  {:<55} -> error={error} ({} bytes, ~{statements} select nodes)", sql, out.len());
    }

    // Interrupting a long query from another thread.
    let handle = conn.interrupt_handle();
    let started = Instant::now();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        handle.interrupt();
    });
    let r = conn.query_row("SELECT count(*) FROM range(100000000000) a", [], |r| r.get::<_, i64>(0));
    println!("interrupt: {:?} after {:?}", r.err().map(|e| e.to_string()), started.elapsed());
    assert!(started.elapsed() < Duration::from_secs(5));
}
