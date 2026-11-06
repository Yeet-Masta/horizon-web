# Horizon Web

**Minecraft: Java Edition 26.3 in a browser tab.** [Horizon](https://github.com/ludovic111/horizon),
the game rebuilt in Rust, compiled to WebAssembly, drawn with WebGPU and run on Web Workers.

[![Web](https://github.com/Yeet-Masta/horizon-web/actions/workflows/web.yml/badge.svg?branch=web)](https://github.com/Yeet-Masta/horizon-web/actions/workflows/web.yml)

This is the `web` branch: the same game code as the native client, merged with upstream
Horizon regularly. What's browser-specific lives in [`web/`](web/) (the page, build scripts,
tools) and in `horizon-platform` (files, threads, clocks, HTTP and the allocator for the web).

> **You need your own copy of Minecraft: Java Edition.** The page never downloads game files:
> it plays on resources prepared from your own install. Never put those resources, or a page
> serving them, anywhere public.

## What you get

- **Single player**, the world ticking on its own server thread, as natively.
- **Multiplayer** on Java servers through a small WebSocket relay (`horizon-web-relay`), with
  Microsoft sign-in.
- **Play with friends** browser to browser (WebRTC), with a relay when a direct link fails.
- **Native-like performance**: chunk generation, lighting and meshing on workers, far terrain,
  occlusion culling, render bundles, and an allocator with a heap per thread.
- Worlds and settings saved in the browser (OPFS), resource packs, screenshots, the system
  clipboard, fullscreen with keyboard lock, and sound.

## Browsers

| Browser | Version |
|---|---|
| Chrome, Edge | 113+ |
| Firefox | 141+ (Windows) |
| Safari | 26+ |

The default build runs on threads, which needs a cross-origin isolated page: `web/serve.py` sends
the headers, and on hosts that can't, `coi.js` adds them. The page checks everything and says
what's missing.

## Quick start

```sh
# 1. Prepare the resources from your own Minecraft (the 26.3 server jar), then zip them for the page.
cargo run --release -p horizon-prepare -- --prepare --server-jar ~/Downloads/server.jar
web/pack-resources.sh

# 2. Build: nightly Rust, and the wasm-bindgen CLI from the commit Cargo.lock pins.
cargo install wasm-bindgen-cli --git https://github.com/wasm-bindgen/wasm-bindgen --rev 39ae823bbc14c9d0a4a2869361483b5453a9b133 --locked --force
web/build.sh             # on Windows: web\build.bat

# 3. Play at http://localhost:8080/
python3 web/serve.py
```

| Build option | What it does |
|---|---|
| *(none)* | wasm32 with threads, release, panics that unwind |
| `--wasm64` | 64-bit memory: up to 8 GiB instead of 4, a little slower (Chrome/Edge 133+, Firefox 134+) |
| `--no-threads` | one thread, for hosts where the page can't be cross-origin isolated |
| `--fast` | quicker to build (no LTO), a little slower to run |
| `--profiling` | keeps function names, for a browser profiler |

With `wasm-opt` (binaryen) and `brotli` installed, the module is optimized further and served
at about a fifth of its size.

## Tips

- **Smoothest frames**: set Video Settings → Max Framerate to *Unlimited*. The browser never
  draws faster than your screen anyway, and a cap below its refresh rate makes frames uneven.
- **Laptops with two GPUs**: Windows decides which one the browser uses (Settings → System →
  Display → Graphics).
- **Chasing a stutter**: `?env=HORIZON_SPIKES=25` logs every slow frame with where its time
  went, and `tools/web_profile.py` records a full report (frames, CPU, GPU, a trace).

## More

- [`web/README.md`](web/README.md): everything in detail, from hosting, multiplayer and the relay
  to profiling, what differs from the native game, and merging upstream.
- [Horizon](https://github.com/ludovic111/horizon) and [playhorizon.dev](https://playhorizon.dev):
  the native game, its documentation and downloads.

MIT licensed ([LICENSE](LICENSE)); third-party code is listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Not affiliated with Mojang or Microsoft.
