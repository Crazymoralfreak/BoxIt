// The version of BoxIt is written in three files: package.json,
// src-tauri/tauri.conf.json and the [workspace.package] table of Cargo.toml.
// They must agree, and a release tag must name the same version.
//
//   node scripts/check-version.mjs           the three files agree
//   node scripts/check-version.mjs v0.0.1    ... and match this tag

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (...parts) => readFileSync(join(root, ...parts), "utf8");

const workspace = /^\[workspace\.package\][^[]*?^version\s*=\s*"([^"]+)"/ms.exec(read("Cargo.toml"));
const versions = {
  "package.json": JSON.parse(read("package.json")).version,
  "src-tauri/tauri.conf.json": JSON.parse(read("src-tauri", "tauri.conf.json")).version,
  "Cargo.toml": workspace?.[1],
};

const expected = process.argv[2]?.replace(/^v/, "") ?? versions["package.json"];
const wrong = Object.entries(versions).filter(([, version]) => version !== expected);
if (wrong.length > 0) {
  for (const [file, version] of wrong) {
    console.error(`${file}: version ${version ?? "is missing"}, expected ${expected}`);
  }
  process.exit(1);
}
console.log(`Version ${expected} in ${Object.keys(versions).join(", ")}.`);
