# BoxIt

[Русский](README.ru.md)

An archive manager. The window, the command line and the self-extracting stub share one engine: an extraction is the same all-or-nothing extraction, with the same checks of the names and the same limits. A damaged archive does not leave a half-written folder.

The interface is available in German, English, Spanish, French, Japanese, Korean, Polish, Portuguese, Russian, Turkish and Simplified Chinese. BoxIt has no account. It reaches the network only when you press **Check for updates**.

## Download

The latest version is on the [releases](https://github.com/Crazymoralfreak/BoxIt/releases/latest) page.

Windows, x64:

- `BoxIt_<version>_x64-setup.exe` — the installer. It installs BoxIt for the current user, no administrator rights needed.
- `BoxIt_<version>_x64-portable.zip` — the portable version. Unpack the `BoxIt` folder anywhere and start `BoxIt.exe`. The empty file `portable` beside the program makes it keep settings, logs and temporary files in the `data` folder next to itself.

A tag also builds a Windows ARM64 installer, macOS disk images (Apple Silicon and Intel) and Linux `.deb` and AppImage packages (x64 and ARM64). The Explorer menu is Windows only. On macOS the disk image is signed locally, without an Apple certificate, so Gatekeeper asks before the first open.

The files are not signed with a code-signing certificate, so Windows SmartScreen shows a warning on the first start.

## What it can do

- **Open.** ZIP, 7-Zip, TAR, GZIP, BZIP2, XZ, Zstandard, LZ4, RAR, CAB, ISO, AR, CPIO, LHA and WIM. A name such as `notes.tar.gz` is a compressed tar, not a single gzip file. Drag and drop, a dialog, or a file passed on the command line.
- **Create.** ZIP, 7-Zip, TAR and the compressed tar and single-file formats above. Compression level, method, dictionary, threads, solid archives, a comment, and splitting into volumes.
- **Passwords.** ZIP and 7-Zip can be encrypted, and the names can be hidden as well. A saved password goes into the Windows Credential Manager, not into the data folder. On macOS and Linux the password is asked again.
- **Extract.** The whole archive, one folder, or the selection. A new folder named after the archive, the folder the archive is in, or a folder you choose. When a file already exists: ask, skip, replace, or keep both. Symbolic links are created only when they stay inside the destination.
- **Test and repair.** A test checks every file. A repair writes what still reads into a new archive and leaves the damaged one as it is.
- **Change an archive.** Add or replace files. Convert from one format to another without unpacking to disk. Compare two archives by name, size and checksum. Search by a substring, a `*` / `?` mask or a `re:` regular expression. Estimate the size of a new archive from a sample.
- **Several at once.** A queue with pause and cancel. Test or extract a list of archives. Two panes, a preview (F3) and editing a file from the archive (F4).
- **Self-extracting archives.** A Windows `.exe` built from a ZIP or 7-Zip archive and `boxit-sfx.exe`.
- **Windows.** File associations and a menu in Explorer, including the Windows 11 menu: open, extract here, extract to a folder, add to an archive, compress and email. A mark of the web on an archive is copied onto the files extracted from it. The installed copy can update itself; the portable copy only says that a new version is out.

## Command line

`BoxIt.exe` is also the command line. When its first argument is a command, it runs the command without opening the window. `boxit-cli` is the same program with a console, so a terminal waits for it and it can ask for a password.

```
BoxIt list photos.7z
BoxIt extract photos.7z -o photos
BoxIt create backup.tar.zst documents projects --level maximum
BoxIt test backup.tar.zst
BoxIt repair broken.zip -o fixed.zip
BoxIt help
```

Other commands: `update`, `convert`, `diff`, `find`, `estimate`, `batch`, `bench`.

- `--json` prints the result for scripts. The text printed for people is in the language of the interface and may change between versions.
- Exit codes: `0` done, `1` failed and nothing is left half done, `2` the command line is not understood, `3` done but some entries were left out, `130` stopped with Ctrl+C.
- `BoxIt.exe` is a window program, so an interactive `cmd` or PowerShell does not wait for it. In a script, wait for it explicitly: `start /wait BoxIt.exe ...` in `cmd`, or pipe the output in PowerShell. `boxit-cli` does not need that.

## Requirements

- Windows 10 or Windows 11, x64 or ARM64. The WebView2 runtime: Windows 11 has it, and the installer brings it when it is missing. The portable copy does not start without it.
- macOS 11 or newer, Apple Silicon or Intel.
- Linux x64 or ARM64 with GTK and WebKitGTK 4.1.

## Data

Settings, the list of recent archives and the logs are in `%AppData%\BoxIt` on Windows, `~/Library/Application Support/BoxIt` on macOS, and `$XDG_DATA_HOME/BoxIt` (or `~/.local/share/BoxIt`) on Linux. A portable copy keeps them in the `data` folder beside the program. Only one window runs at a time; a second start hands its files to the first.

## Building

You need Node.js 20.19 or newer (or 22.12 and newer) and Rust 1.98 or newer.

```
npm install
npm run tauri build
```

On Windows the installer is `target\release\bundle\nsis\BoxIt_<version>_x64-setup.exe`. A portable copy is the folder `portable\BoxIt`: `BoxIt.exe`, `boxit-cli.exe`, `boxit-sfx.exe`, `boxit_shell.dll` and an empty file named `portable`. Git ignores that folder.

```
npm run portable
```

## Releasing a version

1. Put the new version number into `package.json`, `src-tauri/tauri.conf.json` and the root `Cargo.toml` (`[workspace.package]`).
2. Commit the changes and tag the commit `v<version>`, for example `v0.0.1`.
3. Push the tag to GitHub.

The tag starts `.github/workflows/release.yml`. It builds the Windows x64 installer, the portable archive and `latest.json`, and the desktop packages for the other systems. `latest.json` is what an installed copy reads. The portable copy does not replace itself.

The application installs only an update whose signature matches the public key in `src-tauri/tauri.conf.json` (`plugins.updater.pubkey`). The private key belongs in the GitHub Actions secret `TAURI_SIGNING_PRIVATE_KEY`. Signing is turned on by `src-tauri/tauri.release.conf.json`, which only the release build uses.

## Checks

```
npm run typecheck
npm run i18n:check
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo test --workspace
```

`scripts/check-i18n.mjs` checks that every dictionary has the same keys and the same `{placeholders}` as `ru.json`. `crates/boxit-engine` checks the same thing, and that every error code has a text.

## Layout

- `crates/boxit-core` — the archive model, names, limits. No files.
- `crates/boxit-security` — names, links, the destination folder.
- `crates/boxit-codecs` — reading and writing the formats. The only crate with `unsafe`.
- `crates/boxit-engine` — opening, extracting, creating, the queue, settings, texts.
- `crates/boxit-cli` — the command line.
- `crates/boxit-sfx` — the Windows self-extractor.
- `crates/boxit-shell` — the Windows 11 Explorer menu.
- `src-tauri` — the window.
- `src` — the interface in React and TypeScript. Dictionaries are in `src/i18n`.

## Licence

MIT, see [LICENSE](LICENSE). The licences of the libraries BoxIt is built from are in [THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).
