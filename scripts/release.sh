#!/usr/bin/env bash
# Build a static release binary of a Ferrix app: x86_64-unknown-linux-musl, everything linked in (SQLite, FTS5,
# sqlite-vec, aws-lc for TLS, zstd), the web app embedded, no runtime dependencies. Copy it to any x86-64 Linux box
# (or a `FROM scratch` image) and run it.
#
#   scripts/release.sh                        # todo-server, the reference app → dist/todo-server
#   PACKAGE=my-server WEB=web scripts/release.sh   # your app: its cargo package and its web app directory
#
# Environment:
#   PACKAGE   cargo package to build (default: todo-server)
#   WEB       directory of the web app to build first with `bun run build` ("" to skip; default: examples/todo/web
#             for todo-server). The app embeds its dist/ with rust-embed (see examples/todo/server/src/main.rs).
#   TARGET    target triple (default: x86_64-unknown-linux-musl; aarch64-unknown-linux-musl works the same way)
#   PROFILE   cargo profile (default: dist = release + LTO, one codegen unit, stripped)
#   FEATURES  cargo features of the package to enable (e.g. `olap`; default: none)
#   OUT       output directory (default: dist)
#
# Toolchain: C code (SQLite, sqlite-vec, aws-lc, zstd) is cross-compiled for musl with `zig cc` through
# cargo-zigbuild. If `zig` and `cargo-zigbuild` are on PATH they are used; otherwise they are run through mise
# (https://mise.jdx.dev), which downloads them on first use.
set -euo pipefail
cd "$(dirname "$0")/.."

PACKAGE="${PACKAGE:-todo-server}"
TARGET="${TARGET:-x86_64-unknown-linux-musl}"
PROFILE="${PROFILE:-dist}"
OUT="${OUT:-dist}"
if [[ -z "${WEB+set}" ]]; then
  if [[ "$PACKAGE" == "todo-server" ]]; then WEB=examples/todo/web; else WEB=""; fi
fi
ZIG_VERSION=0.15.2
ZIGBUILD_VERSION=0.23.4

# 1. The web app (embedded at compile time).
if [[ -n "$WEB" ]]; then
  echo "==> building the web app in $WEB"
  bun install --frozen-lockfile
  bun run build                       # the @ferrix/* client packages
  (cd "$WEB" && bun run build)
  # rust-embed reads dist/ when the crate compiles: recompile the app crate (only) so it embeds the fresh files.
  cargo clean --quiet -p "$PACKAGE" --profile "$PROFILE" --target "$TARGET" || true
fi

# 2. The Rust target.
if command -v rustup >/dev/null 2>&1; then
  rustup target add "$TARGET" >/dev/null
fi

# 3. Cross-compile. The bundled SQLite gets its build flags (LIBSQLITE3_FLAGS) from .cargo/config.toml: never set that
# variable here, it would replace them (and serialize readers again). The CFLAGS below are for sqlite-vec only:
# sqlite-vec declares `uint8_t` & co. through the BSD types `u_int8_t`, which musl's headers only
# provide with _BSD_SOURCE; map them to the standard types for the C code of this target.
target_env="$(echo "$TARGET" | tr '-' '_')"
export "CFLAGS_${target_env}=-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t"
build=(cargo zigbuild --locked --profile "$PROFILE" -p "$PACKAGE" --target "$TARGET")
if [[ -n "${FEATURES:-}" ]]; then
  build+=(--features "$FEATURES")
fi
echo "==> ${build[*]}"
if zig version >/dev/null 2>&1 && cargo-zigbuild --version >/dev/null 2>&1; then
  "${build[@]}"
elif command -v mise >/dev/null 2>&1; then
  mise exec "zig@${ZIG_VERSION}" "github:rust-cross/cargo-zigbuild@${ZIGBUILD_VERSION}" -- "${build[@]}"
else
  echo "error: needs zig and cargo-zigbuild on PATH, or mise to fetch them" >&2
  exit 1
fi

# 4. Collect and check.
bin="${CARGO_TARGET_DIR:-target}/$TARGET/$PROFILE/$PACKAGE"
mkdir -p "$OUT"
cp "$bin" "$OUT/$PACKAGE"
if command -v file >/dev/null 2>&1; then
  file "$OUT/$PACKAGE"
fi
if [[ "$TARGET" == x86_64-* && "$(uname -m)" == x86_64 ]]; then
  "$OUT/$PACKAGE" --help >/dev/null   # it runs
fi
size=$(stat -c %s "$OUT/$PACKAGE" 2>/dev/null || stat -f %z "$OUT/$PACKAGE")
echo "==> $OUT/$PACKAGE: $size bytes ($((size / 1024 / 1024)) MiB)"
