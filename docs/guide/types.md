# Rust and TypeScript types

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

Things to watch for:

- **64-bit integers are `number`.** JavaScript numbers are exact only up to 2^53 (9 007 199 254 740 991): a larger
  `i64` or `u64` (a snowflake id, an external API's id, nanoseconds) arrives rounded, and nothing warns. Send such
  values as `String`.
- **Field names are camelCase** in `#[table]` and `#[object]` types and in function arguments (`created_at` is
  `createdAt`), also when you write them by hand, as in `ctx.db.patch(id, json!({ "createdAt": now }))`. The fields of
  an enum's struct variants keep their Rust names unless the enum sets `#[serde(rename_all_fields = "camelCase")]`.
- **Function names stay as written** (`api.todos.set_done`).
- **Your own types need `#[object]`** (or hand-written `Serialize`, `Deserialize` and `ts_rs::TS` derives) to appear in
  a function signature; the compiler says so when one is missing.
