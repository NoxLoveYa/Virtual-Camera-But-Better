// Writes src-tauri/src/fx/defaults.json from the filter catalogue: { "<kind>": { "<param>": default, ... } }.
// Rust checks every registered kind against this file (cargo test -p better-cam fx). Run it after editing
// src/filters/schema.ts:  npm run schema:export
// Node runs the .ts file itself (built-in type stripping, on by default since 22.18), so no extra package is needed.
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

const { FILTERS, defaultParams } = await import("../src/filters/schema.ts");

const defaults = Object.fromEntries(Object.keys(FILTERS).map((kind) => [kind, defaultParams(kind)]));
const out = fileURLToPath(new URL("../src-tauri/src/fx/defaults.json", import.meta.url));
writeFileSync(out, JSON.stringify(defaults, null, 2) + "\n");
console.log(`wrote ${out} (${Object.keys(defaults).length} filter kinds)`);
