//! Zero-downtime takeover (`serve --takeover`, `ferrix::ops::takeover`), in one process: an "old" server serving
//! and a "new" one taking over the same data directory. Under write load nothing is refused or lost, WebSockets are
//! told to reconnect (1012), the commit ts carries on, and a second takeover works too. A new version that can't
//! serve the data (a changed type that existing documents fail, or documents the old server writes during the
//! takeover) gives up and the old server carries on, with its journal off again. An interrupted swap is finished.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ferrix::ops::DirLock;
use ferrix::ops::takeover::{self, Control};
use ferrix::ops::tls::{self, Listen};
use ferrix::{App, AppOptions, FunctionRef, TableDef};
use futures::StreamExt;
use serde_json::{Value, json};

use crate::common::todos;

/// A server as `serve` runs it: lock, app, listener, control socket; it stops when a new process takes over.
struct Serving {
    app: App,
    addr: SocketAddr,
    stopped: tokio::task::JoinHandle<()>,
}

async fn serve(dir: &Path, lock: DirLock, app: App, listen: Listen, inherited: tls::Inherited) -> Serving {
    let server = tls::start_with(ferrix::server::router(app.clone()), &listen, inherited).await.unwrap();
    let addr = server.addr;
    let control = Control::start(app.clone(), dir, server.listener_fds()).await.unwrap();
    let pauser = server.pauser();
    let a = app.clone();
    let stopped = tokio::spawn(async move {
        server
            .run_until(async {
                control.handover_requested().await;
                a.begin_drain();
                pauser.stop_accepting();
                tokio::time::sleep(Duration::from_millis(100)).await;
                a.begin_close();
            })
            .await
            .unwrap();
        a.logs().flush().await.unwrap();
        a.db().seal().await.unwrap();
        drop(control);
        drop(a);
        drop(lock);
    });
    Serving { app, addr, stopped }
}

async fn start_old(dir: &Path) -> Serving {
    let lock = DirLock::acquire(dir).unwrap();
    let app = App::open(dir, AppOptions { dev: true, ..Default::default() }).await.unwrap();
    serve(dir, lock, app, Listen::Http("127.0.0.1:0".parse().unwrap()), Vec::new()).await
}

/// Take over `dir` with `tables` and serve on the inherited socket.
async fn take_over(dir: &Path, addr: SocketAddr, tables: Vec<TableDef>) -> ferrix::Result<Serving> {
    let incoming = takeover::take_over_with(dir, tables).await?;
    let options = AppOptions { dev: true, prechecked_schema: Some(incoming.migrating), ..Default::default() };
    let app = App::open(dir, options).await?;
    Ok(serve(dir, incoming.lock, app, Listen::Http(addr), incoming.listeners).await)
}

async fn add(client: &reqwest::Client, addr: SocketAddr, text: &str) -> Result<Value, String> {
    let res = client
        .post(format!("http://{addr}/api/mutation"))
        .json(&json!({ "path": todos::add::path(), "args": { "text": text } }))
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    let body: Value = res.json().await.map_err(|e| format!("{e}"))?;
    if body["status"] == "success" { Ok(body) } else { Err(body.to_string()) }
}

async fn count(app: &App) -> usize {
    app.query::<todos::count>(()).await.unwrap()
}

/// The registered tables with `todos` replaced by `with`.
fn tables_with(todos_def: TableDef) -> Vec<TableDef> {
    ferrix::runtime::registered_tables()
        .into_iter()
        .map(|t| if t.name == "todos" { todos_def.clone() } else { t })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_under_load_refuses_nothing_loses_nothing_and_can_happen_again() {
    let dir = tempfile::tempdir().unwrap();
    let old = start_old(dir.path()).await;
    let addr = old.addr;
    for i in 0..50 {
        add(&reqwest::Client::new(), addr, &format!("before {i}")).await.unwrap();
    }

    // A WebSocket client: told to reconnect (1012) when the old server hands over.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/sync")).await.unwrap();
    let ws_closed = tokio::spawn(async move {
        while let Some(frame) = ws.next().await {
            if let Ok(tokio_tungstenite::tungstenite::Message::Close(Some(close))) = frame {
                return u16::from(close.code);
            }
        }
        0
    });

    // Writers for the whole takeover, each on its own keep-alive connection.
    let ok = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (ok, failures, stop) = (ok.clone(), failures.clone(), stop.clone());
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let mut n = 0;
                while !stop.load(Ordering::SeqCst) {
                    match add(&client, addr, &format!("w{w} {n}")).await {
                        Ok(_) => ok.fetch_add(1, Ordering::SeqCst),
                        Err(e) => {
                            failures.lock().unwrap().push(e);
                            0
                        }
                    };
                    n += 1;
                }
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let new = take_over(dir.path(), addr, ferrix::runtime::registered_tables()).await.unwrap();
    old.stopped.await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.store(true, Ordering::SeqCst);
    for w in writers {
        w.await.unwrap();
    }
    let failures = failures.lock().unwrap().clone();
    assert!(failures.is_empty(), "{} request(s) failed: {:?}", failures.len(), &failures[..failures.len().min(3)]);
    assert_eq!(count(&new.app).await, 50 + ok.load(Ordering::SeqCst), "every acknowledged write is there");
    assert_eq!(ws_closed.await.unwrap(), 1012);
    assert!(!dir.path().join(takeover::STAGING_DIR).join("data.db").exists());
    assert!(dir.path().join(takeover::STAGING_DIR).join("previous").join("data.db").exists());

    // The new server carries on (commit ts after the old one's), and can itself be taken over.
    let before = count(&new.app).await;
    add(&reqwest::Client::new(), addr, "after the takeover").await.unwrap();
    let newer = take_over(dir.path(), addr, ferrix::runtime::registered_tables()).await.unwrap();
    new.stopped.await.unwrap();
    assert_eq!(count(&newer.app).await, before + 1);
    add(&reqwest::Client::new(), addr, "after the second takeover").await.unwrap();
    assert_eq!(count(&newer.app).await, before + 2);
}

/// `todos` with a stricter type: documents whose text starts with `legacy:` don't match. `changed_type` also changes
/// the rendered type (so the startup schema check revalidates the table).
fn stricter_todos(changed_type: bool) -> TableDef {
    let mut def = <todos::Todo as ferrix::Table>::table_def();
    def.validate = |v| {
        if v["text"].as_str().is_some_and(|t| t.starts_with("legacy:")) {
            return Err("`legacy:` texts are not allowed any more".into());
        }
        ferrix::__private::validate_doc::<todos::Todo>(v)
    };
    if changed_type {
        def.ts = || "{ text: string; done: boolean; createdBy: string | null; /* v2 */ }".to_string();
    }
    def
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_version_that_cant_serve_the_data_gives_up_and_the_old_server_carries_on() {
    let dir = tempfile::tempdir().unwrap();
    let old = start_old(dir.path()).await;
    let client = reqwest::Client::new();
    add(&client, old.addr, "legacy: an old document").await.unwrap();

    let err = take_over(dir.path(), old.addr, tables_with(stricter_todos(true))).await.err().expect("refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("can't take over") && msg.contains("legacy:"), "{msg}");
    // Still serving, journal off again, nothing left behind; a later takeover can happen.
    add(&client, old.addr, "still served").await.unwrap();
    assert!(!old.app.db().journal().is_enabled());
    assert!(!dir.path().join(takeover::STAGING_DIR).exists());
    assert!(!old.stopped.is_finished());
    let new = take_over(dir.path(), old.addr, ferrix::runtime::registered_tables()).await.unwrap();
    old.stopped.await.unwrap();
    assert_eq!(count(&new.app).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn documents_written_during_the_takeover_are_checked_against_the_new_type() {
    let dir = tempfile::tempdir().unwrap();
    let old = start_old(dir.path()).await;
    let addr = old.addr;
    for i in 0..20 {
        add(&reqwest::Client::new(), addr, &format!("fine {i}")).await.unwrap();
    }
    // The old server keeps writing documents the new type refuses while the new process catches up. (Same rendered
    // type: the copy's schema check passes; the replayed documents are what fail.)
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    let writer = tokio::spawn(async move {
        let client = reqwest::Client::new();
        let mut n = 0;
        while !s.load(Ordering::SeqCst) {
            let _ = add(&client, addr, &format!("legacy: {n}")).await;
            n += 1;
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let err = take_over(dir.path(), addr, tables_with(stricter_todos(false))).await.err().expect("refused");
    stop.store(true, Ordering::SeqCst);
    writer.await.unwrap();
    let msg = format!("{err:#}");
    assert!(msg.contains("doesn't match the table's type"), "{msg}");
    add(&reqwest::Client::new(), addr, "still served").await.unwrap();
    assert!(!old.stopped.is_finished());
}

#[test]
fn an_interrupted_swap_is_finished_on_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join(takeover::STAGING_DIR);
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("data.db"), b"the new copy").unwrap();
    std::fs::write(staging.join("swapping"), b"").unwrap();
    takeover::recover(dir.path()).unwrap();
    assert_eq!(std::fs::read(dir.path().join("data.db")).unwrap(), b"the new copy");
    assert!(!staging.join("swapping").exists());
    // Nothing to do the next time.
    takeover::recover(dir.path()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_opens_a_copy_like_a_takeover_while_the_server_runs() {
    let dir = tempfile::tempdir().unwrap();
    let old = start_old(dir.path()).await;
    add(&reqwest::Client::new(), old.addr, "x").await.unwrap();
    let report = takeover::check(dir.path()).await.unwrap();
    assert!(report.ts > 0 && report.migrating.is_empty(), "{report:?}");
    assert!(
        std::fs::read_dir(dir.path()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with("check-"))
    );
    // A takeover without a running server says so.
    drop(old);
    let other = tempfile::tempdir().unwrap();
    let err = takeover::take_over(other.path()).await.err().unwrap();
    assert!(format!("{err:#}").contains("no running server to take over"), "{err:#}");
}

/// A takeover onto data with a pending `#[migration(on_read)]`: the table stays migrating (the schema check isn't
/// repeated), typed reads work at once, and the migration runs on the new server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_takeover_keeps_a_pending_migration_pending_and_reads_working() {
    use crate::migrations::m::{add_note, note_tags, notes};
    let dir = tempfile::tempdir().unwrap();
    {
        let app = App::open(dir.path(), AppOptions { dev: true, ..Default::default() }).await.unwrap();
        for (title, tag) in [("a", "x"), ("b", "y")] {
            app.mutation::<add_note>((title.to_string(), vec![tag.to_string()])).await.unwrap();
        }
    }
    crate::migrations::rewrite_documents(dir.path(), "mig_notes", |doc| {
        let tag = doc["tags"][0].clone();
        doc.remove("tags");
        doc.insert("tag".into(), tag);
    });
    let old = start_old(dir.path()).await;
    assert!(old.app.db().catalog().is_migrating("mig_notes"));
    let new = take_over(dir.path(), old.addr, ferrix::runtime::registered_tables()).await.unwrap();
    old.stopped.await.unwrap();
    assert!(new.app.db().catalog().is_migrating("mig_notes"));
    let expected = vec![("a".to_string(), vec!["x".to_string()]), ("b".to_string(), vec!["y".to_string()])];
    assert_eq!(new.app.query::<notes>(()).await.unwrap(), expected);
    let r = new.app.run_migration::<note_tags>(Default::default()).await.unwrap();
    assert_eq!(r.schema_check.as_deref(), Some("passed"));
    assert!(!new.app.db().catalog().is_migrating("mig_notes"));
}
