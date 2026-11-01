# Developing ItchiAss(ets)

Tauri 2 app with a Rust backend in `src-tauri/` and a SvelteKit frontend in `src/`.

## Running

1. `bun install`
2. `bun run tauri dev`

The itch.io OAuth app's redirect URI is `http://127.0.0.1/callback`, and any loopback port is accepted.
Set `ITCHIASS_CLIENT_ID` at build time to use a different OAuth app.

Run the backend tests with `cargo test` inside `src-tauri/`, and type check the frontend with `bun run check`.

## How downloads are stored

Downloads land in `<folder>/<creator>/<title>/`, or `<folder>/<title>/` with the creator setting off.
`.itchiass.json` in the download folder records which version of each file you have, which is what update checks compare against.
Zip, tar.gz, 7z and rar archives are extracted into a folder named after the archive.

## Linux icon in dev

Wayland takes window icons from an installed desktop entry, so `bun run tauri dev` shows a generic icon until you install one:

```sh
install -Dm644 app-icon/icon.png ~/.local/share/icons/hicolor/1024x1024/apps/dev.mark.itchiass.png
install -Dm644 app-icon/dev.mark.itchiass.desktop ~/.local/share/applications/dev.mark.itchiass.desktop
```

## Building locally

`bun run appimage` builds a Linux AppImage and copies it to `ItchiAssets.AppImage` in the project folder.
If linuxdeploy fails on strip, run it as `NO_STRIP=true bun run appimage`.

## Releasing

1. Bump `version` in `src-tauri/tauri.conf.json` and commit it to `main`.
2. Tag that commit `v<version>` (for example `v0.2.0`) and push the tag.

`.github/workflows/release.yml` then builds Windows, macOS (Apple Silicon and Intel) and Linux and publishes a GitHub release.
The build stops early if the tag isn't on `main` or doesn't match the version in `tauri.conf.json`.
Release files are named `ItchiAssets.exe`, `ItchiAssets_AppleSilicon.dmg`, `ItchiAssets_Intel.dmg`, `ItchiAssets.AppImage` and `ItchiAssets.deb`, which keeps the README download links stable.
The in-app updater reads `latest.json` from the newest release of `bearlikelion/ItchiAssets`.

One-time setup:

1. The updater signing key lives at `~/.tauri/itchiassets.key`, with its public half in `tauri.conf.json`. Keep a backup, since losing it means installed copies can never update again.
2. Add repository secrets `TAURI_SIGNING_PRIVATE_KEY` (the contents of that key file) and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (empty).

macOS and Windows builds are unsigned, so both warn on first launch until signing certificates are added to the workflow.
