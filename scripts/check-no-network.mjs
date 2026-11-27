// An archiver has no reason to talk to the network. This check makes that a
// property of the build: no crate under crates/ (the archive logic, the
// command line, the shell extension) may depend, directly or through others,
// on a crate that opens connections. Only the window may, for the update
// check that the user starts with a button.
//
//   node scripts/check-no-network.mjs

import { spawnSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

// HTTP clients, TLS stacks, name resolvers and raw sockets.
const NETWORK = new Set([
  "reqwest",
  "hyper",
  "hyper-util",
  "h2",
  "h3",
  "quinn",
  "ureq",
  "minreq",
  "attohttpc",
  "isahc",
  "surf",
  "curl",
  "curl-sys",
  "rustls",
  "native-tls",
  "openssl",
  "openssl-sys",
  "schannel",
  "security-framework",
  "tungstenite",
  "hickory-resolver",
  "trust-dns-resolver",
  "socket2",
  "async-net",
  "ssh2",
  "suppaftp",
]);

function packageName(manifest) {
  const match = /^\[package\][^[]*?^name\s*=\s*"([^"]+)"/ms.exec(readFileSync(manifest, "utf8"));
  if (!match) {
    throw new Error(`${manifest} has no package name`);
  }
  return match[1];
}

const packages = readdirSync(join(root, "crates"))
  .map((folder) => join(root, "crates", folder, "Cargo.toml"))
  .filter((manifest) => existsSync(manifest))
  .map(packageName);
if (packages.length === 0) {
  console.error("No crates were found under crates/.");
  process.exit(1);
}

const problems = [];
for (const name of packages) {
  // Every target and every feature: a connection must not appear on another
  // system or behind a switch either. Dependencies of tests do not count.
  const result = spawnSync(
    "cargo",
    ["tree", "--locked", "-p", name, "-e", "normal,build", "--target", "all", "--all-features", "--prefix", "none"],
    { cwd: root, encoding: "utf8" },
  );
  if (result.status !== 0) {
    console.error(result.stderr);
    process.exit(result.status ?? 1);
  }
  const found = new Set();
  for (const line of result.stdout.split("\n")) {
    const crate = line.trim().split(" ")[0];
    if (NETWORK.has(crate)) {
      found.add(crate);
    }
  }
  if (found.size > 0) {
    problems.push(`${name} depends on ${[...found].sort().join(", ")}`);
  }
}

if (problems.length > 0) {
  console.error(problems.join("\n"));
  console.error("\nThe crates under crates/ must not be able to open a network connection.");
  process.exit(1);
}
console.log(`No network crates in the dependencies of ${packages.join(", ")}.`);
