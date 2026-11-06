// Runs a wasm32-wasip1 program under Node's WASI, with the working directory
// preopened as `.`: node tools/run_wasi.mjs probe.wasm [args...]
// tools/java_conformance.py runs the interpreter's wasm build through this.
import { WASI } from 'node:wasi';
import { readFile } from 'node:fs/promises';

const [wasmPath, ...args] = process.argv.slice(2);
const wasi = new WASI({
  version: 'preview1',
  args: ['horizon-java-run', ...args],
  preopens: { '.': '.' },
  returnOnExit: true,
});
const { instance } = await WebAssembly.instantiate(await readFile(wasmPath), wasi.getImportObject());
process.exitCode = wasi.start(instance);
