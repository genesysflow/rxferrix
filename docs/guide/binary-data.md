# Binary data

`rxferrix::Bytes` holds small binary values (a hash, a key, a thumbnail, an encoded protobuf) in documents, function
args and results. For anything larger than a few hundred KB, or that users download, use
[file storage](/guide/file-storage) instead: a document is read whole every time, and requests are limited to 2 MiB.

```rust
use rxferrix::prelude::*;

#[table(name = "keys")]
#[index(by_fingerprint = [fingerprint])]
pub struct Key {
    pub name: String,
    pub fingerprint: Bytes,
}

#[mutation]
pub async fn add_key(ctx: &MutationCtx, name: String, fingerprint: Bytes) -> Result<Id<Key>> {
    ensure_user!(fingerprint.len() == 32, "bad_fingerprint", "a fingerprint is 32 bytes");
    ctx.db.insert(Key { name, fingerprint }).await
}

#[query]
pub async fn by_fingerprint(ctx: &QueryCtx, fingerprint: Bytes) -> Result<Option<Doc<Key>>> {
    ctx.db.query::<Key>().with_index(Key::by_fingerprint, |q| q.eq(Key::fingerprint, fingerprint)).first().await
}
```

`Bytes` dereferences to `[u8]` and converts from `Vec<u8>` and `&[u8]`.

**On the wire and in storage** it is `{"$bytes": "<base64>"}`, so in TypeScript it is `{ $bytes: string }`. Convert with
`bytesToJson(uint8Array)` and `bytesFromJson(value)` from `@rxferrix/client` (`isBytes(value)` tells whether a value is
one):

```ts
import { bytesFromJson, bytesToJson } from "@rxferrix/client";

const id = await addKey({ name: "laptop", fingerprint: bytesToJson(digest) });
const key = useQuery(api.keys.by_fingerprint, { fingerprint: bytesToJson(digest) });
const raw: Uint8Array | undefined = key ? bytesFromJson(key.fingerprint) : undefined;
```

**In indexes** `Bytes` sort byte by byte, after strings (see the order of types in [Design](../DESIGN.md#storage)), so a
range over a `Bytes` field is a prefix or byte-range scan. Base64 adds a third to the stored size; keep index keys
short (keys over about 1 KB spill into overflow pages and slow scans down).
