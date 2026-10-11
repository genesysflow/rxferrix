//! The relevance score of text search: the FTS5 auxiliary function `ferrix_score(<fts table>)`.
//!
//! ## Why not FTS5's `bm25()`
//!
//! `bm25()` weighs each document with corpus statistics: the number of documents, their average length and how many
//! of them contain each term. Any write to the index changes those numbers, and with them the score (and possibly the
//! order) of *every* document. That would make two things unsound:
//! - per-filter-value invalidation: a search in channel `c` would have to rerun after a write in channel `d`, since
//!   that write can reorder `c`'s results;
//! - pagination: page boundaries are positions in score order, and a write anywhere would move documents across them.
//!
//! So the score is BM25's per-document part only: for each query phrase `i` with `tf_i` occurrences in a document of
//! `dl` tokens,
//!
//! ```text
//! score = Σ_i tf_i · (k1 + 1) / (tf_i + k1 · (1 − b + b · dl / L))      k1 = 1.2, b = 0.75, L = 32
//! ```
//!
//! i.e. BM25 without the IDF factor and with a fixed reference length `L` instead of the corpus average. It depends
//! only on the document's text and the query: more occurrences rank higher (with saturation), and shorter documents
//! rank higher for the same occurrences. Every term must match anyway (queries are ANDs), so IDF would only re-weigh
//! terms that all documents in the result contain; losing it costs little and makes a document's position change
//! only when that document changes. Scores are computed in a fixed order of `f64` operations, so they are
//! bit-for-bit reproducible, which cursors rely on.

use std::ffi::{c_char, c_int, c_void};
use std::ptr;

use rusqlite::ffi;

/// BM25 term-frequency saturation.
pub const K1: f64 = 1.2;
/// BM25 length normalization.
pub const B: f64 = 0.75;
/// The fixed reference document length (in tokens) that replaces BM25's corpus average.
pub const REFERENCE_LENGTH: f64 = 32.0;
/// At most this many phrases are scored (queries have at most `MAX_SEARCH_TERMS` = 32 terms).
const MAX_PHRASES: usize = 64;

/// The score of one document, from the occurrences of each phrase and the document length (see the module docs).
pub fn score(tf: &[u32], doc_len: u32) -> f64 {
    let norm = K1 * (1.0 - B + B * f64::from(doc_len) / REFERENCE_LENGTH);
    let mut sum = 0.0;
    for &t in tf {
        let t = f64::from(t);
        sum += t * (K1 + 1.0) / (t + norm);
    }
    sum
}

/// `ferrix_score(fts)`: the score of the current row of an FTS5 full-text query.
unsafe extern "C" fn ferrix_score(
    api: *const ffi::Fts5ExtensionApi,
    fts: *mut ffi::Fts5Context,
    ctx: *mut ffi::sqlite3_context,
    _n_args: c_int,
    _args: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: FTS5 calls this with a valid API table and context for the current row.
    let result = unsafe { row_score(&*api, fts) };
    // SAFETY: `ctx` is the function's result context.
    unsafe {
        match result {
            Ok(s) => ffi::sqlite3_result_double(ctx, s),
            Err(code) => ffi::sqlite3_result_error_code(ctx, code),
        }
    }
}

unsafe fn row_score(api: &ffi::Fts5ExtensionApi, fts: *mut ffi::Fts5Context) -> Result<f64, c_int> {
    let check = |rc: c_int| if rc == ffi::SQLITE_OK { Ok(()) } else { Err(rc) };
    let (Some(phrase_count), Some(inst_count), Some(inst), Some(column_size)) =
        (api.xPhraseCount, api.xInstCount, api.xInst, api.xColumnSize)
    else {
        return Err(ffi::SQLITE_MISUSE);
    };
    // SAFETY (all calls below): FTS5 API functions called with the context FTS5 passed in, and valid out-pointers.
    let phrases = (unsafe { phrase_count(fts) }.max(0) as usize).min(MAX_PHRASES);
    let mut tf = [0u32; MAX_PHRASES];
    let mut n = 0;
    check(unsafe { inst_count(fts, &mut n) })?;
    for i in 0..n {
        let (mut phrase, mut col, mut off) = (0, 0, 0);
        check(unsafe { inst(fts, i, &mut phrase, &mut col, &mut off) })?;
        // Column 0 is the text; the others (filter values) are UNINDEXED and never match.
        if col == 0 && (0..phrases as c_int).contains(&phrase) {
            tf[phrase as usize] += 1;
        }
    }
    let mut len = 0;
    check(unsafe { column_size(fts, 0, &mut len) })?;
    Ok(score(&tf[..phrases], len.max(0) as u32))
}

/// Auto-extension entry point: registers `ferrix_score` and the `ferrix_snowball` tokenizer with the connection's
/// FTS5 module.
unsafe extern "C" fn register(
    db: *mut ffi::sqlite3,
    _err: *mut *mut c_char,
    _api: *const ffi::sqlite3_api_routines,
) -> c_int {
    // The documented way to get the fts5_api pointer: `SELECT fts5(?1)` with ?1 bound to a pointer to it.
    let mut stmt = ptr::null_mut();
    // SAFETY: plain SQLite C API calls on the connection being opened; the bound pointer outlives the statement.
    unsafe {
        // The statement runs while the connection opens, before its owner can set a busy timeout: without one, opening
        // a WAL database while another connection to it closes (and checkpoints) fails with "database is locked".
        // Owners set their own timeout afterwards, which replaces this one.
        ffi::sqlite3_busy_timeout(db, 5000);
        let rc = ffi::sqlite3_prepare_v2(db, c"SELECT fts5(?1)".as_ptr(), -1, &mut stmt, ptr::null_mut());
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        let mut fts5: *mut ffi::fts5_api = ptr::null_mut();
        ffi::sqlite3_bind_pointer(stmt, 1, (&mut fts5 as *mut *mut ffi::fts5_api).cast::<c_void>(), c"fts5_api_ptr".as_ptr(), None);
        ffi::sqlite3_step(stmt);
        let rc = ffi::sqlite3_finalize(stmt);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        let Some(create) = fts5.as_ref().and_then(|a| a.xCreateFunction) else {
            return ffi::SQLITE_ERROR;
        };
        let rc = create(fts5, c"ferrix_score".as_ptr(), ptr::null_mut(), Some(ferrix_score), None);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        // The Snowball tokenizer (`super::stem`), on the same connection: tokenizers are per connection too.
        super::stem::register(fts5)
    }
}

/// Register `ferrix_score` (and `ferrix_snowball`) for every SQLite connection opened afterwards (called once, from
/// [`super::register_extensions`]).
pub(super) fn register_auto_extension() {
    type Init = unsafe extern "C" fn(*mut ffi::sqlite3, *mut *mut c_char, *const ffi::sqlite3_api_routines) -> c_int;
    let init: Init = register;
    // SAFETY: registering an auto extension is thread-safe; `register` has the entry point signature.
    unsafe {
        ffi::sqlite3_auto_extension(Some(init));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn more_occurrences_and_shorter_documents_rank_higher() {
        assert!(score(&[3], 5) > score(&[1], 2));
        assert!(score(&[1], 2) > score(&[1], 5));
        assert!(score(&[2], 10) > score(&[1], 10));
        assert!(score(&[1, 1], 4) > score(&[1], 4));
        assert_eq!(score(&[], 4), 0.0);
        // Saturation: each extra occurrence adds less.
        assert!(score(&[2], 8) - score(&[1], 8) > score(&[3], 8) - score(&[2], 8));
    }

    /// Opening a connection runs the extension's `SELECT fts5(?1)` before the caller can set a busy timeout. While
    /// another connection to the same WAL database closes (the last one checkpoints and deletes the WAL under an
    /// exclusive lock), that must wait, not fail the open with "database is locked".
    #[test]
    fn opening_while_another_connection_closes_waits() {
        super::super::register_extensions();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let setup = rusqlite::Connection::open(&path).unwrap();
        setup.pragma_update(None, "journal_mode", "WAL").unwrap();
        setup.execute_batch("CREATE TABLE t (x BLOB)").unwrap();
        drop(setup);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closer = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let Ok(conn) = rusqlite::Connection::open(&path) else { continue };
                    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
                    let _ = conn.execute("INSERT INTO t VALUES (zeroblob(65536))", []);
                }
            })
        };
        let opened = (0..300).map(|_| rusqlite::Connection::open(&path).map(drop)).collect::<Result<Vec<_>, _>>();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        closer.join().unwrap();
        opened.unwrap();
    }

    #[test]
    fn registered_function_matches_the_formula() {
        super::super::register_extensions();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE f USING fts5(t, x UNINDEXED);
             INSERT INTO f (rowid, t, x) VALUES (1, 'hello world', 'a'), (2, 'hello hello hello wonderful world', 'b'),
                                                (3, 'nothing here', 'hello');",
        )
        .unwrap();
        let rows: Vec<(i64, f64)> = conn
            .prepare("SELECT rowid, ferrix_score(f) AS sc FROM f WHERE f MATCH '\"hello\" \"wor\" *' ORDER BY sc DESC, rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(2, score(&[3, 1], 5)), (1, score(&[1, 1], 2))]);
        // Usable in WHERE through its alias, e.g. for cursors.
        let n: i64 = conn
            .query_row("SELECT count(*) FROM (SELECT rowid, ferrix_score(f) AS sc FROM f WHERE f MATCH 'hello' AND sc < ?1)", [score(&[3], 5)], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
