# Test backend behavior

`rxferrix::testing::TestApp` opens a fresh temporary data directory and runs the registered backend functions in-process. Use it to test validation, access checks, transactions, and delayed work without a separate server.

Declare test modules in your app as usual. Compiled table/function modules must be included so their macros register them. For `#[tokio::test]`, add Tokio as a development dependency with `macros` and `rt-multi-thread` features if it is not already available.

## Call typed functions

For the starter app's `tasks` module:

```rust
use rxferrix::testing::TestApp;
use rxferrix::UserError;

#[tokio::test]
async fn adding_a_task() {
    let t = TestApp::new().await.unwrap();
    let id = t.mutation::<crate::tasks::add>("Buy milk".to_string())
        .await.unwrap();
    let tasks = t.query::<crate::tasks::list>(()).await.unwrap();
    assert_eq!(tasks[0].id, id);
    assert_eq!(tasks[0].text, "Buy milk");

    let error = t.mutation::<crate::tasks::add>(String::new())
        .await.unwrap_err();
    assert_eq!(UserError::of(&error).map(|e| e.code.as_str()), Some("empty_text"));
}
```

This uses the generated starter's no-argument `list`. If you added the guide's optional filter argument, pass `None::<bool>` instead of `()`.

Arguments follow function parameter order: `()` for none, a value or `(value,)` for one, and a tuple for several. Results have their Rust types; inspect `.id` on a `Doc<T>` rather than the client's `_id`.

## Test as an app user

```rust
let ann = t.create_user("ann@example.com", None).await.unwrap();
let result = t.as_user(ann).query::<crate::notes::list>(()).await.unwrap();
```

`create_user` creates a verified user directly; it does not require a configured sign-in method or email delivery. Pass `Some(password)` when a test needs a password account.

Test both allowed and refused calls, including signed-out callers and another user's document IDs. Configure `AppOptions::guard` when testing an app-wide guard. The [chat example's tests](https://github.com/genesysflow/rxferrix/blob/master/examples/chat-react/server/src/tests.rs) show membership and ownership checks.

## Advance jobs without sleeping

The scheduler does not run automatically in a default `TestApp`. `t.run_due_jobs().await?` runs due work; `t.advance_time(Duration::from_secs(60)).await?` advances the test clock and runs work that becomes due.

Use these helpers for delays, expiry, crons, and retries. For code that truly requires a background scheduler, opt in with `start_scheduler()`.

## Exercise restarts and HTTP

| Helper | Purpose |
| --- | --- |
| `TestApp::with_options(AppOptions { ... })` | Configure runtime options, guards, and services. |
| `TestApp::with_auth(config)` | Configure sign-in behavior for auth tests. |
| `t.restart().await?` | Reopen the same data directory and check persisted state. |
| `t.serve().await?` | Start HTTP and WebSocket handling on an available local port. |
| `t.query_run::<f>(args)` | Inspect a query run and its tracked reads. |
| `t.mutation_ts::<f>(args)` | Get a mutation result and commit timestamp. |
| `t.app()` | Access the lower-level runtime. |

`TestApp::new()` and `with_auth()` enable development mode, including detailed errors and dev-only functions. `with_options()` uses the options you supply, so set `dev: true` when that is what the test needs. For JavaScript binding tests, `@rxferrix/client/testing` provides a fake sync server; the client packages use it in their test suites.
