# Small binary values

Use `rxferrix::Bytes` for small binary values that belong inside a document or function call, such as a hash, key, or encoded message. Use [file storage](/guide/file-storage) for uploads and downloadable files.

A document is read as a whole and the ordinary API has a 2 MiB request limit. Base64 also adds about a third to the binary size, so large bytes make documents and subscriptions more expensive.

## Define a binary field

```rust
use rxferrix::prelude::*;

#[table(name = "keys")]
#[index(by_fingerprint = [fingerprint])]
pub struct Key {
    pub name: String,
    pub fingerprint: Bytes,
}

#[mutation]
pub async fn add(ctx: &MutationCtx, name: String, fingerprint: Bytes) -> Result<Id<Key>> {
    ensure_user!(fingerprint.len() == 32, "bad_fingerprint", "A fingerprint is 32 bytes");
    ctx.db.insert(Key { name, fingerprint }).await
}
```

`Bytes` converts from `Vec<u8>` and `&[u8]`, and dereferences to `[u8]`. It serializes as `{ "$bytes": "base64" }`; an ordinary `Vec<u8>` is a JSON array instead.

## Convert on the client

```ts
import { bytesFromJson, bytesToJson } from "@rxferrix/client";
import { api } from "./rxferrix";

await client.mutation(api.keys.add, {
  name: "laptop",
  fingerprint: bytesToJson(digest),
});

// Given a Key document returned by a query:
const raw: Uint8Array = bytesFromJson(key.fingerprint);
```

Here `digest` is a `Uint8Array`. `isBytes(value)` checks the JSON representation, and generated TypeScript uses `{ $bytes: string }`.

## Index bytes

A `Bytes` field can be indexed and compared with typed index constraints. Its order is byte-by-byte, rather than the lexical order of its base64 encoding. Keep indexed binary values short; large keys can require extra B-tree pages.

See [Types](/guide/types) for the other Rust-to-TypeScript mappings and [storage design](/DESIGN#storage) for key encoding.
