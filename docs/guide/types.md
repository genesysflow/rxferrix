# Rust and TypeScript types

Rust is the source of truth for both stored data and function signatures. Regenerate the client API after changing either. In a generated web project, restart `cargo run -- serve --dev`; otherwise run your app’s `codegen --out DIR` command.

## Generated files

| File | Contents |
| --- | --- |
| `api.ts` | Typed references to public functions, plus `devApi` for development-only functions. |
| `dataModel.ts` | Table documents, IDs, custom objects, and shared argument/result types. |
| `index.ts` | Re-exports for importing from the generated directory. |

Do not edit these files manually. Function references carry argument and output types; they are used by all client bindings.

## Value mapping

The generated `api.ts` and `dataModel.ts` give every function's arguments and result a TypeScript type. Values travel
as JSON; this is how each Rust type looks on the client.

| Rust | JSON | TypeScript |
|---|---|---|
| `bool` | `true` | `boolean` |
| `i8`…`i64`, `u8`…`u64`, `f32`, `f64` | `42`, `1.5` | `number` |
| `String`, `&str`, `char` | `"text"` | `string` |
| `Option<T>` | the value or `null` | `T \| null` |
| `Option<T>` as a function parameter | may be left out | `field?: T \| null` |
| `Option<T>` with `#[serde(skip_serializing_if = "Option::is_none")]` | left out when `None` | `field?: T \| null` |
| `Vec<T>`, `HashSet<T>`, `BTreeSet<T>` | array | `Array<T>` |
| `HashMap<String, T>`, `BTreeMap<String, T>` | object | `{ [key in string]: T }` |
| `Id<T>` | `"26 characters"` | `Id<"table">` |
| `Doc<T>` | the fields plus `_id`, `_creationTime` | `Doc<"table">` |
| `Bytes` | `{ "$bytes": "base64" }` | `{ $bytes: string }` ([Binary data](/guide/binary-data)) |
| `serde_json::Value` | any | `JsonValue` |
| a struct with `#[object]` | object, camelCase keys | a named type |
| an enum with `#[object]`, unit variants | `"inProgress"` | `"inProgress" \| ...` |
| an enum with `#[object]`, data variants | `{ "variant": ... }` | a union |
| `chrono::DateTime<Utc>`, `NaiveDate`, ... (feature `chrono`) | `"2026-10-29T12:30:00Z"` | `string` |
| `uuid::Uuid` (feature `uuid`) | `"…-…"` | `string` |

## Naming and precision

- **64-bit integers are `number`.** JavaScript numbers are exact only up to 2^53 (9 007 199 254 740 991): a larger
  `i64` or `u64` (a snowflake id, an external API's id, nanoseconds) arrives rounded, and nothing warns. Send such
  values as `String`.
- **Field names are camelCase** in `#[table]` and `#[object]` types and in function arguments (`created_at` is
  `createdAt`), also when you write them by hand, as in `ctx.db.patch(id, json!({ "createdAt": now }))`. The fields of
  an enum's struct variants keep their Rust names unless the enum sets `#[serde(rename_all_fields = "camelCase")]`.
- **Function names stay as written** (`api.todos.set_done`).
- **Your own types need `#[object]`** (or hand-written `Serialize`, `Deserialize` and `ts_rs::TS` derives) to appear in
  a function signature; the compiler says so when one is missing.

## Define custom objects

Use `#[object]` on structs and enums appearing in function arguments or results when they are not tables:

```rust
use rxferrix::prelude::*;

#[object]
pub struct TaskSummary {
    pub open_count: u32,
    pub latest_text: Option<String>,
}
```

The client gets a named `TaskSummary` type with `openCount` and `latestText`. The macro provides serde and TypeScript derives through framework re-exports. Field doc comments become generated JSDoc.

For dates and UUIDs, enable the `chrono` or `uuid` feature on `rxferrix` and add the corresponding crate to your app to name its types. Use serde support as needed. Dates serialize as strings according to the type's serde format; `DateTime<Utc>` uses RFC 3339. `ctx.now()` remains `f64` epoch milliseconds.

When hand-writing patches, use serialized field names. When calling functions, keep their Rust names: `set_done` is `api.tasks.set_done`, while a `completed_at` argument becomes `completedAt`.
