//! Phase 11 spike (throwaway): a DuckDB round trip reachable from a binary, so the linker keeps DuckDB and the
//! static build can be measured and run.

/// Open an in-memory DuckDB, append rows, aggregate them, and check a statement with `json_serialize_sql`.
pub fn probe() -> crate::Result<String> {
    let conn = duckdb::Connection::open_in_memory()?;
    conn.execute_batch("CREATE TABLE t (k VARCHAR, v DOUBLE)")?;
    {
        let mut appender = conn.appender("t")?;
        for i in 0..100_000 {
            appender.append_row(duckdb::params![format!("k{}", i % 10), i as f64])?;
        }
    }
    let (groups, total): (i64, f64) =
        conn.query_row("SELECT count(DISTINCT k), sum(v) FROM t", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let check: String = conn.query_row("SELECT json_serialize_sql('DELETE FROM t')", [], |r| r.get(0))?;
    let version: String = conn.query_row("SELECT version()", [], |r| r.get(0))?;
    Ok(format!("duckdb {version}: {groups} groups, sum {total}, DELETE rejected: {}", check.contains("\"error\":true")))
}
