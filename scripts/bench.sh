#!/usr/bin/env bash
# Run benchmarks (the ignored `bench_*` integration tests of rxferrix) in release mode, one at a time.
#
#   scripts/bench.sh bench_network                                   # host target (glibc on Linux)
#   TARGET=x86_64-unknown-linux-musl scripts/bench.sh bench_network  # musl, the target of scripts/release.sh
#   scripts/bench.sh bench_writes bench_parallel_reads               # several filters
#
# The arguments are test-name filters (default: every benchmark). Tunables are environment variables documented in
# each benchmark (BENCH_SECS, BENCH_CONNS, ...).
#
# Environment:
#   TARGET    target triple (default: the host's). A musl target is cross-compiled with `zig cc` through
#             cargo-zigbuild like scripts/release.sh (from PATH, or through mise), so the C code (SQLite and the rest)
#             is built and linked as in the release binaries. Uses the release profile, not dist: LTO doesn't change
#             what is measured here enough to be worth its build time.
#   FEATURES  cargo features of rxferrix to enable (default: none beyond its defaults)
set -euo pipefail
cd "$(dirname "$0")/.."

ZIG_VERSION=0.15.2
ZIGBUILD_VERSION=0.23.4
filters=("$@")
if [[ ${#filters[@]} -eq 0 ]]; then filters=(bench_); fi

cmd=(test --release --locked -p rxferrix --test it)
if [[ -n "${FEATURES:-}" ]]; then cmd+=(--features "$FEATURES"); fi

if [[ -z "${TARGET:-}" ]]; then
  run=(cargo "${cmd[@]}")
else
  cmd+=(--target "$TARGET")
  if command -v rustup >/dev/null 2>&1; then rustup target add "$TARGET" >/dev/null; fi
  # As in scripts/release.sh: sqlite-vec's BSD integer types, which musl's headers only declare with _BSD_SOURCE.
  target_env="$(echo "$TARGET" | tr '-' '_')"
  export "CFLAGS_${target_env}=-Du_int8_t=uint8_t -Du_int16_t=uint16_t -Du_int64_t=uint64_t"
  if zig version >/dev/null 2>&1 && cargo-zigbuild --version >/dev/null 2>&1; then
    run=(cargo-zigbuild "${cmd[@]}")
  elif command -v mise >/dev/null 2>&1; then
    run=(mise exec "zig@${ZIG_VERSION}" "github:rust-cross/cargo-zigbuild@${ZIGBUILD_VERSION}" -- cargo-zigbuild "${cmd[@]}")
  else
    echo "error: a TARGET needs zig and cargo-zigbuild on PATH, or mise to fetch them" >&2
    exit 1
  fi
fi

for filter in "${filters[@]}"; do
  echo "==> ${filter} (${TARGET:-host})"
  "${run[@]}" "$filter" -- --ignored --nocapture --test-threads 1
done
