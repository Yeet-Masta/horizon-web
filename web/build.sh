#!/usr/bin/env bash
# Builds the web version of the game into web/dist (web/README.md).
#
#   web/build.sh                 wasm32 with threads, the default
#   web/build.sh --wasm64        wasm64 (memory64): up to 8 GiB of memory instead of 4, and slower
#                                (wasm32 measured 7-14% faster: memory64 bounds-checks every access)
#   web/build.sh --no-threads    one thread: no shared memory, so no COOP/COEP headers needed
#   web/build.sh --fast          the quicker-to-build `fast` profile (no LTO) instead of release
#   web/build.sh --config=FILE   adds a cargo config (rustflags, say), as web/simd.toml does
#   web/build.sh --abort         panics abort the thread they happen on instead of unwinding
#                                (web/unwind.toml; the one-thread wasm32 build always aborts)
#   web/build.sh --profiling     keeps the module's function names, which wasm-opt drops, for a
#                                profiler (web/README.md, "Profiling"): 8 MB more, 2 MB downloaded
#   web/build.sh --sccache       compiles through sccache (RUSTC_WRAPPER), which keeps compiled
#                                crates between builds and checkouts: not the game's own crate nor
#                                its link, nor `fast`'s incremental crates
#   web/build.sh --cranelift     compiles build scripts and proc macros with the Cranelift backend
#                                (nightly; rustup adds it). Cranelift can't make WebAssembly: the
#                                game itself is still LLVM's
#
# Every build uses WebAssembly SIMD and tail calls (web/simd.toml), which every browser with
# WebGPU has. Threads and wasm64 build std here, with nightly Rust (rustup adds rust-src). Needs the
# wasm-bindgen CLI from the commit Cargo.lock pins (the error says the command); wasm-opt
# (binaryen) is used when it is on the PATH. Serve the result with web/serve.py.
# HORIZON_MSA_CLIENT_ID in the environment builds in another Azure app for Microsoft sign-in
# than Horizon's own.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
target=wasm32-unknown-unknown
profile=release
threads=1
unwind=1
names=0
sccache=0
cranelift=0
extra=()
for arg in "$@"; do
  case "$arg" in
    --wasm32) target=wasm32-unknown-unknown ;;
    --wasm64) target=wasm64-unknown-unknown ;;
    --no-threads) threads=0 ;;
    --threads) threads=1 ;;
    --release) profile=release ;;
    --fast) profile=fast ;;
    --config=*) extra+=(--config "${arg#--config=}") ;;
    --abort) unwind=0 ;;
    --profiling) names=1 ;;
    --sccache) sccache=1 ;;
    --cranelift) cranelift=1 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

cd "$root"
# wasm-bindgen comes from its main branch for now (Cargo.toml's [patch.crates-io]): the CLI
# must be built from the same commit.
pinned="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)"
rev="$(awk '/^name = "wasm-bindgen"$/ { getline; getline; if (match($0, /#[0-9a-f]+/)) print substr($0, RSTART + 1, RLENGTH - 1); exit }' Cargo.lock)"
have="$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }' || true)"
install="cargo install wasm-bindgen-cli --version $pinned --locked"
[ -n "$rev" ] && install="cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev $rev --locked --force"
if [ "$have" != "$pinned" ]; then
  echo "wasm-bindgen $pinned${rev:+ (wasm-bindgen main, $rev)} is needed (found: ${have:-none}):" >&2
  echo "  $install" >&2
  exit 1
fi

if [ "$sccache" = 1 ]; then
  if ! command -v sccache >/dev/null; then
    echo "--sccache: sccache isn't on the PATH (cargo install sccache --locked)" >&2
    exit 1
  fi
  export RUSTC_WRAPPER=sccache
fi
# Cranelift for what runs on this machine during the build: build scripts, proc macros and what
# they use (the profile's build-override); cargo's codegen-backend option is nightly's.
backend=()
if [ "$cranelift" = 1 ]; then
  rustup component add rustc-codegen-cranelift-preview --toolchain nightly >/dev/null
  backend=(-Zcodegen-backend)
  export "CARGO_PROFILE_$(echo "$profile" | tr '[:lower:]-' '[:upper:]_')_BUILD_OVERRIDE_CODEGEN_BACKEND=cranelift"
fi

# No sound, controllers or mod runtime in a browser (web/README.md).
args=(-p horizon-client --bin horizon-client --no-default-features --target "$target" --profile "$profile"
  --config web/simd.toml "${extra[@]}")
# Builds that make std (threads, wasm64) can unwind; the one-thread wasm32 build, on stable's
# prebuilt std, aborts.
unwinds=0
if [ "$threads" = 1 ] || [ "$target" = wasm64-unknown-unknown ]; then
  [ "$unwind" = 1 ] && unwinds=1
  # wasm64 is a tier 3 target, and threads need std built with atomics: std is built here,
  # with the nightly toolchain. web/threads.toml adds shared memory and atomics.
  rustup component add rust-src --toolchain nightly >/dev/null
  rustup target add "$target" --toolchain nightly >/dev/null 2>&1 || true
  [ "$threads" = 1 ] && args+=(--config web/threads.toml)
  # Panics unwind (web/unwind.toml) unless --abort: std is built with what that needs.
  panic_std=panic_unwind
  if [ "$unwinds" = 1 ]; then args+=(--config web/unwind.toml); else panic_std=panic_abort; fi
  cargo +nightly build -Zbuild-std=std,$panic_std "${backend[@]}" "${args[@]}"
elif [ "$cranelift" = 1 ]; then
  # On nightly for its codegen-backend option, with the prebuilt std as stable's.
  rustup target add wasm32-unknown-unknown --toolchain nightly >/dev/null
  cargo +nightly build "${backend[@]}" "${args[@]}"
else
  rustup target add wasm32-unknown-unknown >/dev/null
  cargo build "${args[@]}"
fi
if [ "$sccache" = 1 ]; then
  sccache --show-stats | grep -E '^(Compile requests|Cache hits|Cache misses|Non-cacheable calls) ' || true
fi

dist="$here/dist"
rm -rf "$dist"
mkdir -p "$dist/pkg" "$dist/fonts"
wasm="${CARGO_TARGET_DIR:-target}/$target/$profile/horizon-client.wasm"
if ! wasm-bindgen --target web --no-typescript --out-dir "$dist/pkg" "$wasm"; then
  echo "If wasm-bindgen says the module's bindgen format differs, install the pinned one:" >&2
  echo "  $install" >&2
  exit 1
fi
if command -v wasm-opt >/dev/null; then
  flags=(-O2 --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext --enable-mutable-globals --enable-reference-types --enable-multivalue --enable-simd --enable-tail-call --enable-extended-const)
  [ "$target" = wasm64-unknown-unknown ] && flags+=(--enable-memory64)
  [ "$threads" = 1 ] && flags+=(--enable-threads)
  [ "$unwinds" = 1 ] && flags+=(--enable-exception-handling)
  [ "$names" = 1 ] && flags+=(-g)
  wasm-opt "${flags[@]}" "$dist/pkg/horizon-client_bg.wasm" -o "$dist/pkg/horizon-client_bg.wasm"
else
  echo "wasm-opt not found: the module is not optimized further (install binaryen to shrink it)" >&2
fi

cp "$here/index.html" "$here/horizon.js" "$here/worker.js" "$here/coi.js" "$dist/"
cp "$here/apache.htaccess" "$dist/.htaccess"
cp docs/media/brand/web/favicon.svg "$dist/"
cp crates/horizon-theme/fonts/Geist-Regular.ttf crates/horizon-theme/fonts/PixelifySans-SemiBold.ttf \
  crates/horizon-theme/fonts/OFL-*.txt "$dist/fonts/"
commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
threads_json=false
[ "$threads" = 1 ] && threads_json=true
unwind_json=false
[ "$unwinds" = 1 ] && unwind_json=true
printf '{"target":"%s","profile":"%s","threads":%s,"simd":true,"tailCall":true,"unwind":%s,"commit":"%s"}\n' "$target" "$profile" "$threads_json" "$unwind_json" "$commit" > "$dist/build.json"
# Brotli beside each text and module file (`.br`): web/serve.py and the .htaccess serve it to
# browsers that accept it (all current ones), with `Content-Encoding: br`, so the module downloads
# at about a quarter of its size and still compiles as it streams. Quality 11 for a release,
# 9 (seconds, not minutes) for --fast; BROTLI_QUALITY sets it. Without the `brotli` tool the
# files are served as they are.
if command -v brotli > /dev/null; then
  quality="${BROTLI_QUALITY:-$([ "$profile" = release ] && echo 11 || echo 9)}"
  find "$dist" -type f \( -name '*.wasm' -o -name '*.js' -o -name '*.html' -o -name '*.json' -o -name '*.svg' \) \
    -print0 | xargs -0 -n 1 -P "$(nproc 2>/dev/null || echo 2)" brotli -f -k -q "$quality" --lgwin=24
  echo "web/dist: Brotli at quality $quality: $(du -sh "$dist/pkg/horizon-client_bg.wasm.br" | cut -f1) to download"
else
  echo "brotli not found: the module is served uncompressed (install brotli to compress it)" >&2
fi
echo "web/dist: $(du -sh "$dist/pkg/horizon-client_bg.wasm" | cut -f1) module ($target, $profile, $([ "$threads" = 1 ] && echo threads || echo one thread))"
