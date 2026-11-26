// Builds the release program and puts a portable copy of it into
// portable/BoxIt: the program, its command line as a program of its own,
// the licences, and an empty file named "portable" beside them.
// With that file BoxIt keeps its settings, logs and temporary files in the
// "data" folder next to the program and writes nothing anywhere else.
//
// The copy is what gets checked by hand before anything is sent to CI.
// A "data" folder left from an earlier check is kept, so settings survive a
// rebuild.
//
//   node scripts/make-portable.mjs              builds and copies
//   node scripts/make-portable.mjs --no-build   copies what is already built

import { spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const windows = process.platform === "win32";
const config = JSON.parse(readFileSync(join(root, "src-tauri", "tauri.conf.json"), "utf8"));
const program = `${config.mainBinaryName}${windows ? ".exe" : ""}`;
const folder = join(root, "portable", config.productName);
const notices = "THIRD-PARTY-NOTICES.txt";

/** Where cargo puts what it builds: `target` here unless the environment or a config says otherwise. */
function targetFolder() {
  const result = spawnSync("cargo", ["metadata", "--format-version", "1", "--no-deps"], {
    cwd: root,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
  });
  if (result.status !== 0) {
    console.error(result.stderr);
    process.exit(result.status ?? 1);
  }
  return JSON.parse(result.stdout).target_directory;
}

if (!process.argv.includes("--no-build")) {
  // Built first: the installer packs this file, and the portable copy
  // puts it next to BoxIt.exe.
  if (windows) {
    const sfx = spawnSync("cargo", ["build", "--release", "--locked", "-p", "boxit-sfx"], {
      cwd: root,
      stdio: "inherit",
    });
    if (sfx.status !== 0) {
      console.error("The build of the self-extractor failed; the portable copy was not updated.");
      process.exit(sfx.status ?? 1);
    }
    const menu = spawnSync("cargo", ["build", "--release", "--locked", "-p", "boxit-shell"], {
      cwd: root,
      stdio: "inherit",
    });
    if (menu.status !== 0) {
      console.error("The build of the context menu failed; the portable copy was not updated.");
      process.exit(menu.status ?? 1);
    }
  }
  // The Tauri build embeds the interface into the program; a plain
  // `cargo build` would leave it looking for the development server.
  // One command line for the shell: npm is a script on Windows and cannot be
  // started without one.
  const result = spawnSync("npm run tauri -- build --no-bundle", {
    cwd: root,
    stdio: "inherit",
    shell: true,
  });
  if (result.status !== 0) {
    console.error("The build failed; the portable copy was not updated.");
    process.exit(result.status ?? 1);
  }
  // The command line as a program of its own: a terminal waits for it, so
  // it can ask for a password, which the window program cannot.
  const cli = spawnSync("cargo", ["build", "--release", "--locked", "-p", "boxit-cli"], {
    cwd: root,
    stdio: "inherit",
  });
  if (cli.status !== 0) {
    console.error("The build of the command line failed; the portable copy was not updated.");
    process.exit(cli.status ?? 1);
  }
}

const release = join(targetFolder(), "release");
const programs = [program, `boxit-cli${windows ? ".exe" : ""}`];
if (windows) {
  programs.push("boxit-sfx.exe");
  programs.push("boxit_shell.dll");
}
for (const name of programs) {
  if (!existsSync(join(release, name))) {
    console.error(`${join(release, name)} does not exist: build the program first.`);
    process.exit(1);
  }
}

mkdirSync(folder, { recursive: true });
for (const name of programs) {
  copyFileSync(join(release, name), join(folder, name));
}
// The licence of BoxIt and the terms of the code of others inside it go
// with every copy of the program.
copyFileSync(join(root, "LICENSE"), join(folder, "LICENSE.txt"));
copyFileSync(join(root, notices), join(folder, notices));
const marker = join(folder, "portable");
if (!existsSync(marker)) {
  writeFileSync(marker, "");
}

console.log(`Portable copy of ${config.productName} ${config.version} in ${folder}:`);
for (const name of programs) {
  const megabytes = (statSync(join(folder, name)).size / (1024 * 1024)).toFixed(1);
  console.log(`  ${name} (${megabytes} MB)`);
}
