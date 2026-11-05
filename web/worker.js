// One of the threaded web build's threads (web/README.md): the page's module, over the page's
// memory, running the work the game handed this worker (horizon_platform::thread::spawn_worker).
// The module's start (`main`) runs only on the page's thread.
import init, { horizon_thread_entry } from "./pkg/horizon-client.js";

// A thread started from this one: the page starts it (horizon.js), so it outlives this worker.
globalThis.horizonSpawnThread = (name, _module, _memory, work, _transfer) => postMessage({ spawn: [name, work] });
// What goes over the page (a crash on the integrated server's thread, say): the page shows it.
globalThis.horizonStatus = (text, error) => postMessage({ status: [text, error] });
// Work for the page's thread, whose event loop has fetch and the relay's sockets
// (horizon_platform::web::run_on_page): the page runs it by its number.
globalThis.horizonRunOnPage = (task) => postMessage({ page: task });

// JSPI (web/README.md): the game's frames, when its loop runs in this worker, are promising calls,
// in which a wait on the browser suspends the frame (crates/horizon-platform/src/web/jspi.rs).
globalThis.horizonJspiSuspend = async (promise) => {
  try {
    await promise;
    return 0;
  } catch {
    return 1;
  }
};
if (typeof WebAssembly.Suspending === "function" && typeof WebAssembly.promising === "function") {
  const instantiate = WebAssembly.instantiate;
  WebAssembly.instantiate = function (module, imports) {
    for (const space of Object.values(imports ?? {})) {
      for (const name of Object.keys(space)) {
        if (name.includes("horizonJspiSuspend")) {
          space[name] = new WebAssembly.Suspending(space[name]);
          globalThis.horizonJspi = true;
        }
      }
    }
    return instantiate.call(this, module, imports);
  };
}
let wasm;
globalThis.horizonRunFrames = () => {
  const step = WebAssembly.promising(wasm.horizon_frame_step);
  const next = globalThis.requestAnimationFrame
    ? (f) => requestAnimationFrame(f)
    : (f) => setTimeout(f, 4);
  const frame = () => step().then(
    (more) => { if (more) next(frame); },
    (e) => console.error("Horizon: the game's frame failed:", e),
  );
  next(frame);
};

// The page's Blobs (horizon.js): the resources zip and the game folder's larger files, read as
// the game opens them (crates/horizon-platform/src/web/blobs.rs).
const reader = new FileReaderSync();
globalThis.horizonReadBlob = (id, start, end) =>
  new Uint8Array(reader.readAsArrayBuffer(globalThis.horizonBlobs[id].slice(start, end)));

// The game's own thread, when its loop runs in a worker (web/README.md), gets the page's canvas
// (an OffscreenCanvas) and keeps its event loop: its frames run there, as the page's would.
self.onmessage = async ({ data: [module, memory, work, blobs, canvas] }) => {
  self.onmessage = null;
  globalThis.horizonBlobs = blobs ?? [];
  if (canvas) globalThis.horizonCanvas = canvas;
  wasm = await init({ module_or_path: module, memory });
  if (!horizon_thread_entry(work)) close();
};
