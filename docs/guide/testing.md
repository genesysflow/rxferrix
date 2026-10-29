# Testing

`rxferrix::testing::TestApp` runs your app in-process, on a fresh data directory, with the same runtime as `serve`:
functions, reactivity, the scheduler, auth and storage. Call functions with their types.

```rust
use rxferrix::testing::TestApp;
use rxferrix::UserError;

#[tokio::test]
async fn adding_a_todo() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<todos::add>(("Buy milk".to_string(),)).await.unwrap();
    let list = t.query::<todos::list>(()).await.unwrap();
    assert_eq!(list[0].id, id);

    let err = t.mutation::<todos::add>((String::new(),)).await.unwrap_err();
    assert_eq!(UserError::of(&err).map(|e| e.code.as_str()), Some("empty_text"));
}
```

Arguments are a tuple of the function's parameters (`()` for none, `(x,)` for one).

**Users.** `t.create_user(email, Some(password))` creates a verified user directly (no auth method or email needed),
and `t.as_user(id)` calls functions as them:

```rust
let ann = t.create_user("ann@example.com", None).await.unwrap();
t.as_user(ann).mutation::<channels::join>((general,)).await.unwrap();
```

`TestApp::with_auth(AuthConfig::password()...)` configures auth when a test signs in over HTTP; `t.serve()` starts
the HTTP and WebSocket server on a free port and returns its address.

**Options.** `TestApp::with_options(AppOptions { .. })` sets what `serve` flags would (timeouts, scheduler options,
replication, ...). `t.restart()` reopens the same data directory, as a deploy would.

**The scheduler** doesn't run on its own in tests: `t.run_due_jobs()` runs whatever is due (and what that schedules),
and `t.advance_time(Duration::from_secs(60))` moves the test clock forward and runs what became due, so crons, delays
and retries need no sleeping.

**Lower level.** `t.query_run::<f>(args)` returns the run with its read set (to check what a write invalidates),
`t.mutation_ts::<f>(args)` the commit ts, and `t.app()` the `App` for everything else.
