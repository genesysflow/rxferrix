
## Run the frontend

In a second terminal, from this directory:

```sh
cd web
bun install
bun run dev
```

Open http://localhost:5173 in two tabs and add a task. Both lists update. Vite proxies `/api` and its WebSocket to the backend.

Start the backend first: `serve --dev` writes the typed API to `web/src/rxferrix` at startup. Restart it after changing functions or table types. For codegen without serving, run `cargo run -- codegen --out web/src/rxferrix` from the app root. Do not edit the generated files manually.

The production build below embeds `web/dist` in the app binary, so it also serves the frontend.
