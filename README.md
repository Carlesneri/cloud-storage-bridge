# Cloud Storage Bridge

A Tauri desktop app that uploads all media content (images, video, audio) from a
folder you select to a [Cloudflare R2](https://developers.cloudflare.com/r2/) bucket.

## Features

- Native folder picker, recursive scan (hidden files/folders are skipped)
- Media detection by extension: images (jpg, png, heic, webp, ...), video (mp4, mov, mkv, ...)
  and audio (mp3, wav, flac, ...)
- Uploads via R2's S3-compatible API: single PUT for small files, multipart with
  per-part progress and retries for large files
- Per-file and overall progress, kind filters, file selection, cancel support
- Files are stored under their relative folder path, with an optional key prefix

## Getting started

1. Install dependencies and Rust (rustup) if you haven't already:

   ```
   pnpm install
   ```

2. In Cloudflare, create an R2 bucket and an R2 API token with
   **Object Read & Write** permission
   ([docs](https://developers.cloudflare.com/r2/api/tokens/)).

3. Run the app in dev mode:

   ```
   pnpm tauri dev
   ```

4. Fill in Account ID, Bucket, Access Key ID and Secret Access Key, pick a folder,
   and upload.

To build a production binary: `pnpm tauri build`.

## Notes

- Credentials are kept in memory only; Account ID, bucket and prefix are
  remembered in `localStorage` for convenience.
- Advanced: a custom S3 endpoint can be set (defaults to
  `https://<account_id>.r2.cloudflarestorage.com`).
