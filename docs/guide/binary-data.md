# Binary data

`ferrix::Bytes` holds binary values in documents, args and results. It travels and is stored as
`{"$bytes": "<base64>"}` (TS: `{ $bytes: string }`), can be indexed (ordered byte-wise), and converts with
`bytesToJson(uint8Array)` / `bytesFromJson(value)` from `@ferrix/client`. For files, use file storage instead.
