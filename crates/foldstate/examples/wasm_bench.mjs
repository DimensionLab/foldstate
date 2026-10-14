// SPDX-License-Identifier: Apache-2.0
// Times the `wasm_bench` example in Node:
//
//   cargo build -p foldstate --release --example wasm_bench --target wasm32-unknown-unknown
//   node crates/foldstate/examples/wasm_bench.mjs <path to wasm_bench.wasm>
import { readFile } from 'node:fs/promises';

const EVENTS = 200_000;
const bytes = await readFile(process.argv[2]);
const module = await WebAssembly.compile(bytes);
let best = Infinity;
for (let i = 0; i < 5; i++) {
  const { exports } = await WebAssembly.instantiate(module, {});
  const start = performance.now();
  exports.main(0, 0);
  best = Math.min(best, performance.now() - start);
}
console.log(
  `wasm (node ${process.versions.node}): ${best.toFixed(1)} ms for ${EVENTS} appends and a fold (${((best * 1e6) / EVENTS).toFixed(0)} ns per event)`,
);
