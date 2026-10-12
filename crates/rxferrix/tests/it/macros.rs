//! The real `rxferrix-macros` against the real runtime (the other tests use hand-written expansions).

use rxferrix::prelude::*;
use rxferrix::testing::TestApp;
use rxferrix::{FunctionRef, Table};

mod notes {
    use rxferrix::prelude::*;

    /// A note on a board.
    #[table(name = "notes")]
    #[index(by_board = [board, pinned])]
    pub struct Note {
        pub board: String,
        pub body: String,
        pub pinned: bool,
        pub author_name: Option<String>,
    }

    /// Notes on a board, pinned ones first.
    #[query]
    pub async fn on_board(ctx: &QueryCtx, board: String, limit: Option<u32>) -> Result<Vec<Doc<Note>>> {
        ctx.db
            .query::<Note>()
            .with_index(Note::by_board, |q| q.eq(Note::board, board))
            .order(Order::Desc)
            .take(limit.unwrap_or(100) as usize)
            .await
    }

    #[query]
    pub async fn get(ctx: &QueryCtx, id: Id<Note>) -> Result<Option<Doc<Note>>> {
        ctx.db.get(id).await
    }

    #[mutation]
    pub async fn add(ctx: &MutationCtx, board: String, body: String) -> Result<Id<Note>> {
        if body.trim().is_empty() {
            return Err(UserError::new("empty_body", "A note needs some text").into());
        }
        ctx.db.insert(Note { board, body, pinned: false, author_name: None }).await
    }

    #[mutation]
    pub async fn pin(ctx: &MutationCtx, id: Id<Note>) -> Result<()> {
        ctx.db.patch(id, json!({ "pinned": true })).await
    }

    #[internal_query]
    pub async fn count(ctx: &QueryCtx) -> Result<u64> {
        Ok(ctx.db.query::<Note>().collect().await?.len() as u64)
    }

    /// Adds `n` notes through the mutation, then counts them through an internal query.
    #[action]
    pub async fn seed(ctx: &ActionCtx, board: String, n: u32) -> Result<u64> {
        for i in 0..n {
            ctx.run_mutation::<add>((board.clone(), format!("note {i}"))).await?;
        }
        ctx.run_query::<count>(()).await
    }

    /// Local bindings named like functions in this module must not clash with the function references.
    #[mutation]
    pub async fn shadowing(ctx: &MutationCtx, add: String) -> Result<usize> {
        let get = add.len();
        let count = |x: usize| x + get;
        let _ = ctx;
        Ok(count(1))
    }
}

use notes::Note;

#[test]
fn table_metadata_from_the_macro() {
    assert_eq!(Note::NAME, "notes");
    let def = Note::table_def();
    assert_eq!(def.indexes.len(), 1);
    assert_eq!(def.indexes[0].name, "by_board");
    assert_eq!(def.indexes[0].fields, vec!["board", "pinned"]);
    assert_eq!(Note::author_name.name(), "authorName");
    assert_eq!(Note::_creation_time.name(), "_creationTime");
}

#[test]
fn functions_registered_with_paths_and_ts() {
    assert_eq!(notes::on_board::path(), "macros/notes:on_board");
    let list = rxferrix::registry().unwrap().list();
    let f = list.iter().find(|f| f.path == "macros/notes:on_board").expect("registered");
    assert_eq!(f.args_ts, "{ board: string, limit?: number | null, }");
    assert_eq!(f.output_ts, "Array<Doc<\"notes\">>");
    let f = list.iter().find(|f| f.path == "macros/notes:count").expect("registered");
    assert_eq!(f.visibility, rxferrix::__private::Visibility::Internal);
}

#[tokio::test]
async fn end_to_end_through_real_macros() -> Result<()> {
    let t = TestApp::new().await?;

    let a = t.mutation::<notes::add>(("b1".to_string(), "first".to_string())).await?;
    let b = t.mutation::<notes::add>(("b1".to_string(), "second".to_string())).await?;
    t.mutation::<notes::add>(("b2".to_string(), "other board".to_string())).await?;
    t.mutation::<notes::pin>((a,)).await?;

    // Desc over (board, pinned, _id): the pinned note comes first.
    let on_b1 = t.query::<notes::on_board>(("b1".to_string(), None)).await?;
    let bodies: Vec<&str> = on_b1.iter().map(|d| d.body.as_str()).collect();
    assert_eq!(bodies, ["first", "second"]);
    assert!(on_b1[0].pinned);
    assert_eq!(on_b1[1].id, b);

    let limited = t.query::<notes::on_board>(("b1".to_string(), Some(1))).await?;
    assert_eq!(limited.len(), 1);

    let got = t.query::<notes::get>((b,)).await?.expect("exists");
    assert_eq!(got.body, "second");

    // User errors keep their code.
    let err = t.mutation::<notes::add>(("b1".to_string(), "  ".to_string())).await.unwrap_err();
    let user = err.downcast_ref::<UserError>().expect("a UserError");
    assert_eq!(user.code, "empty_body");

    // An action composing a public mutation and an internal query.
    assert_eq!(t.action::<notes::seed>(("b3".to_string(), 5)).await?, 3 + 5);

    assert_eq!(t.mutation::<notes::shadowing>(("abc".to_string(),)).await?, 4);
    Ok(())
}

#[tokio::test]
async fn internal_functions_are_hidden_from_http() -> Result<()> {
    let t = TestApp::new().await?;
    let addr = t.serve().await?;
    let post = |path: &'static str, body: serde_json::Value| async move {
        let body = body.to_string();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    };
    let ok = post("/api/query", json!({ "path": "macros/notes:on_board", "args": { "board": "x" } })).await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let hidden = post("/api/query", json!({ "path": "macros/notes:count", "args": {} })).await;
    assert!(hidden.starts_with("HTTP/1.1 404"), "{hidden}");
    Ok(())
}
