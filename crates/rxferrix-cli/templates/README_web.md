
In a second terminal, start the web app (`serve --dev` writes its typed API to `web/src/rxferrix` on startup, so start
the server first):

```sh
cd web
bun install
bun run dev                  # http://localhost:5173, proxies /api to the server
```
