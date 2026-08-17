# Cloud Storage Bridge

A Tauri desktop app that uploads all media content (images, video, audio) from a
folder you select to a [Cloudflare R2](https://developers.cloudflare.com/r2/) bucket.

Works on **macOS, Windows and Linux**.

## Features

- Native folder picker, recursive scan (hidden files/folders are skipped)
- Media detection by extension: images (jpg, png, heic, webp, ...), video (mp4, mov, mkv, ...),
  and audio (mp3, wav, flac, ...)
- Automatic video preparation (ffmpeg sidecar) so every uploaded video plays in a
  standard browser `<video>` player, with subtitles:
  - H.264 + AAC in MKV/WebM/AVI → remuxed to MP4 (`-c copy`, no re-encode)
  - H.264 with AC3/EAC3/DTS/other audio → video copied, audio transcoded to AAC 192k
  - HEVC/VP9/AV1/other → full transcode to H.264 + AAC, using the GPU
    (VideoToolbox on macOS, NVENC/QuickSync/AMF elsewhere) when available,
    with a CPU `libx264` fallback; CPU transcodes are thread-capped so they
    don't overheat the machine
  - all outputs use `-movflags +faststart` for instant streaming
  - every subtitle track is extracted as a WebVTT sidecar uploaded next to the
    video: `<name>.vtt` (default) and `<name>.<lang>.vtt` (per language);
    image-based subtitles (PGS/VOBSUB) are skipped, and an external `.srt`/`.vtt`
    with the same base name is used instead of embedded tracks when present
  - if ffmpeg fails, the original file is uploaded and flagged
    "may not play in browser"
- Uploads via R2's S3-compatible API: single PUT for small files, multipart with
  per-part progress and retries for large files
- Per-file and overall progress (including a "preparing" phase with ffmpeg
  progress), kind filters, file selection, cancel support
- Files are stored under their relative folder path, with an optional key prefix
- Credentials are remembered between sessions (secret key in the OS keychain)
- Upload history: previously uploaded files start deselected with an "Uploaded" tag;
  files changed since their last upload are flagged "Modified" and stay selected

## Getting started

1. Requirements: [Node.js](https://nodejs.org/), [pnpm](https://pnpm.io/) and
   [Rust](https://rustup.rs/) (rustup), plus the platform prerequisites below.
   Install dependencies:

   ```
   pnpm install
   ```

2. Fetch the bundled ffmpeg/ffprobe sidecar binaries (once per machine; they
   are not committed to the repo):

   ```
   ./scripts/fetch-ffmpeg.sh
   ```

   The script downloads static builds for the current platform into
   `src-tauri/binaries/`. Use `./scripts/fetch-ffmpeg.sh all` to fetch every
   supported target (macOS x86_64/arm64, Linux x86_64/arm64, Windows x86_64)
   before cross-building release bundles.

2. In Cloudflare, create an R2 bucket and an R2 API token with
   **Object Read & Write** permission
   ([docs](https://developers.cloudflare.com/r2/api/tokens/)).

3. Run the app in dev mode:

   ```
   pnpm tauri dev
   ```

4. Fill in Account ID, Bucket, Access Key ID and Secret Access Key, pick a folder,
   and upload.

## Install on your machine

Build a production bundle:

```
pnpm tauri build
```

Installers are written to `src-tauri/target/release/bundle/`:

| Platform | Artifacts | Install |
|---|---|---|
| macOS | `macos/Cloud Storage Bridge.app`, `dmg/*.dmg` | Drag the `.app` to `/Applications`, or open the DMG |
| Windows | `msi/*.msi`, `nsis/*-setup.exe` | Run the installer and follow the wizard |
| Linux | `deb/*.deb`, `rpm/*.rpm`, `appimage/*.AppImage` | `sudo apt install ./<file>.deb` / `sudo dnf install <file>.rpm`, or run the AppImage directly |

### Platform prerequisites (for building)

- **macOS**: Xcode Command Line Tools (`xcode-select --install`)
- **Windows**: [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
  with "Desktop development with C++"; WebView2 is preinstalled on Windows 10/11
- **Linux** (Debian/Ubuntu example):

  ```
  sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
    libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
  ```

### First launch notes (unsigned builds)

- **macOS**: Gatekeeper may block the app — right-click it and choose **Open**
  (or System Settings → Privacy & Security → Open Anyway); it launches normally afterwards.
- **Windows**: SmartScreen may show "Windows protected your PC" — click
  **More info** → **Run anyway**.
- **Linux**: AppImages need the executable bit (`chmod +x <file>.AppImage`).

## Notes

- The secret access key is stored in the OS credential store — macOS Keychain,
  Windows Credential Manager, or Linux Secret Service — never on disk in plain text.
  On first access macOS may ask to allow the app to read the keychain item:
  choose "Always Allow". On Linux, gnome-keyring (or another Secret Service
  implementation) must be running, otherwise the secret is not persisted between
  sessions and must be re-entered.
- Remaining config and upload history live in the app config directory with
  owner-only file permissions (`0600`; `0600`-equivalent on Windows):
  - macOS: `~/Library/Application Support/com.carlesneri.cloud-storage-bridge/`
  - Windows: `%APPDATA%\com.carlesneri.cloud-storage-bridge\`
  - Linux: `~/.config/com.carlesneri.cloud-storage-bridge/`
- Advanced: a custom S3 endpoint can be set (defaults to
  `https://<account_id>.r2.cloudflarestorage.com`).
- Upload history tracks size and modification time per file to detect changes.
