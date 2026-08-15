use std::{
    path::Path,
    sync::Mutex,
    time::Duration,
};

use aws_sdk_s3::{
    config::{Credentials, Region},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use tokio::io::AsyncReadExt;
use walkdir::WalkDir;

const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif", "svg",
    "ico", "jfif",
];
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "avi", "mkv", "webm", "m4v", "mpg", "mpeg", "3gp", "wmv", "flv", "mts", "m2ts",
];
const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "aac", "flac", "ogg", "oga", "m4a", "opus", "wma", "aiff", "aif", "mid", "midi",
];

const SMALL_FILE_LIMIT: u64 = 16 * 1024 * 1024;
const PART_SIZE: u64 = 16 * 1024 * 1024;
const MAX_PARTS: u64 = 10_000;
const MAX_ATTEMPTS: u32 = 3;
const CANCELLED: &str = "cancelled";

#[derive(Clone, Copy, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum MediaKind {
    Image,
    Video,
    Audio,
}

#[derive(Clone, Serialize)]
struct MediaFile {
    path: String,
    relative: String,
    size: u64,
    kind: MediaKind,
}

#[derive(Clone, Deserialize)]
struct R2Config {
    account_id: String,
    access_key_id: String,
    secret_access_key: String,
    bucket: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    endpoint: String,
}

#[derive(Deserialize)]
struct UploadItem {
    path: String,
    relative: String,
    size: u64,
}

#[derive(Clone, Serialize)]
struct FileStartEvent {
    index: usize,
    path: String,
    key: String,
    size: u64,
}

#[derive(Clone, Serialize)]
struct FileProgressEvent {
    index: usize,
    path: String,
    uploaded: u64,
}

#[derive(Clone, Serialize)]
struct FileDoneEvent {
    index: usize,
    path: String,
}

#[derive(Clone, Serialize)]
struct FileErrorEvent {
    index: usize,
    path: String,
    error: String,
}

#[derive(Clone, Serialize)]
struct UploadDoneEvent {
    uploaded: usize,
    failed: usize,
    cancelled: bool,
}

struct UploadState {
    cancel_tx: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
}

fn kind_for_ext(ext: &str) -> Option<MediaKind> {
    if IMAGE_EXTS.contains(&ext) {
        Some(MediaKind::Image)
    } else if VIDEO_EXTS.contains(&ext) {
        Some(MediaKind::Video)
    } else if AUDIO_EXTS.contains(&ext) {
        Some(MediaKind::Audio)
    } else {
        None
    }
}

fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry.depth() > 0
        && entry
            .file_name()
            .to_str()
            .map(|n| n.starts_with('.'))
            .unwrap_or(false)
}

#[tauri::command]
async fn select_folder(app: AppHandle) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .and_then(|p| p.into_path().ok())
            .map(|path| path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
fn scan_folder(path: String) -> Result<Vec<MediaFile>, String> {
    let root = Path::new(&path);
    if !root.is_dir() {
        return Err(format!("`{path}` is not a directory"));
    }

    let mut files = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
    {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(ext) = entry
            .path()
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        else {
            continue;
        };
        let Some(kind) = kind_for_ext(&ext) else {
            continue;
        };
        let size = entry.metadata().map_err(|e| e.to_string())?.len();
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .replace('\\', "/");
        files.push(MediaFile {
            path: entry.path().to_string_lossy().into_owned(),
            relative,
            size,
            kind,
        });
    }

    files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(files)
}

fn emit_progress(app: &AppHandle, index: usize, path: &str, uploaded: u64) {
    let _ = app.emit(
        "upload://progress",
        FileProgressEvent {
            index,
            path: path.to_string(),
            uploaded,
        },
    );
}

fn part_size_for(size: u64) -> u64 {
    PART_SIZE.max(size.div_ceil(MAX_PARTS))
}

async fn with_retry<F, Fut, T>(mut f: F) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(_) if attempt < MAX_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn upload_one(
    app: &AppHandle,
    client: &Client,
    bucket: &str,
    item: &UploadItem,
    index: usize,
    key: &str,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    if *cancel_rx.borrow_and_update() {
        return Err(CANCELLED.to_string());
    }

    let path = Path::new(&item.path);
    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string();

    if item.size <= SMALL_FILE_LIMIT {
        let data = tokio::fs::read(path)
            .await
            .map_err(|e| format!("read error: {e}"))?;
        let len = data.len() as u64;
        with_retry(|| {
            let body = ByteStream::from(data.clone());
            async {
                client
                    .put_object()
                    .bucket(bucket)
                    .key(key)
                    .content_type(&content_type)
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())
            }
        })
        .await?;
        emit_progress(app, index, &item.path, len);
        return Ok(());
    }

    // Multipart upload for large files, with per-part progress and retry.
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open error: {e}"))?;
    let mut reader = tokio::io::BufReader::with_capacity(1024 * 1024, file);
    let part_size = part_size_for(item.size);

    let mpu = with_retry(|| async {
        let result = client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .content_type(&content_type)
            .send()
            .await;
        result.map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("failed to start multipart upload: {e}"))?;

    let upload_id = mpu.upload_id().ok_or("missing upload id")?.to_string();

    let outcome = upload_parts(
        app,
        client,
        bucket,
        key,
        &upload_id,
        &mut reader,
        item,
        index,
        part_size,
        &mut cancel_rx,
    )
    .await;

    match outcome {
        Ok(parts) => {
            let result = with_retry(|| {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts.clone()))
                    .build();
                async {
                    client
                        .complete_multipart_upload()
                        .bucket(bucket)
                        .key(key)
                        .upload_id(&upload_id)
                        .multipart_upload(completed)
                        .send()
                        .await
                        .map_err(|e| e.to_string())
                }
            })
            .await;
            match result {
                Ok(_) => {
                    emit_progress(app, index, &item.path, item.size);
                    Ok(())
                }
                Err(e) => {
                    abort_multipart(client, bucket, key, &upload_id).await;
                    Err(e)
                }
            }
        }
        Err(e) => {
            abort_multipart(client, bucket, key, &upload_id).await;
            Err(e)
        }
    }
}

async fn abort_multipart(client: &Client, bucket: &str, key: &str, upload_id: &str) {
    let _ = client
        .abort_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(upload_id)
        .send()
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn upload_parts(
    app: &AppHandle,
    client: &Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    reader: &mut (impl AsyncReadExt + Unpin),
    item: &UploadItem,
    index: usize,
    part_size: u64,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<CompletedPart>, String> {
    let mut parts = Vec::new();
    let mut uploaded: u64 = 0;
    let mut part_number: i32 = 1;

    while uploaded < item.size {
        if *cancel_rx.borrow_and_update() {
            return Err(CANCELLED.to_string());
        }

        let this_len = part_size.min(item.size - uploaded) as usize;
        let mut buf = vec![0u8; this_len];
        reader
            .read_exact(&mut buf)
            .await
            .map_err(|e| format!("read error: {e}"))?;

        let resp = with_retry(|| {
            let body = ByteStream::from(buf.clone());
            async {
                client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(part_number)
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| e.to_string())
            }
        })
        .await?;

        parts.push(
            CompletedPart::builder()
                .part_number(part_number)
                .e_tag(resp.e_tag().unwrap_or_default().to_string())
                .build(),
        );

        uploaded += this_len as u64;
        emit_progress(app, index, &item.path, uploaded);
        part_number += 1;
    }

    Ok(parts)
}

#[tauri::command]
async fn upload_files(
    app: AppHandle,
    state: State<'_, UploadState>,
    config: R2Config,
    items: Vec<UploadItem>,
) -> Result<UploadDoneEvent, String> {
    if items.is_empty() {
        return Err("No files selected".into());
    }
    let bucket = config.bucket.trim().to_string();
    let access_key = config.access_key_id.trim().to_string();
    let secret = config.secret_access_key.trim().to_string();
    if bucket.is_empty() {
        return Err("Missing bucket name".into());
    }
    if access_key.is_empty() || secret.is_empty() {
        return Err("Missing R2 credentials".into());
    }
    let endpoint = if config.endpoint.trim().is_empty() {
        let account = config.account_id.trim();
        if account.is_empty() {
            return Err("Missing R2 account id".into());
        }
        format!("https://{account}.r2.cloudflarestorage.com")
    } else {
        config.endpoint.trim().to_string()
    };

    let s3_config = aws_sdk_s3::Config::builder()
        .credentials_provider(Credentials::new(
            access_key,
            secret,
            None,
            None,
            "r2-static",
        ))
        .endpoint_url(endpoint)
        .region(Region::new("auto"))
        .force_path_style(true)
        .build();
    let client = Client::from_conf(s3_config);

    let (tx, mut rx) = tokio::sync::watch::channel(false);
    *state.cancel_tx.lock().unwrap() = Some(tx);

    let prefix = config.prefix.trim().trim_matches('/');
    let mut uploaded_count = 0usize;
    let mut failed_count = 0usize;
    let mut cancelled = false;

    for (index, item) in items.iter().enumerate() {
        if *rx.borrow_and_update() {
            cancelled = true;
            break;
        }
        let rel = item.relative.trim_start_matches('/');
        let key = if prefix.is_empty() {
            rel.to_string()
        } else {
            format!("{prefix}/{rel}")
        };

        let _ = app.emit(
            "upload://file-start",
            FileStartEvent {
                index,
                path: item.path.clone(),
                key: key.clone(),
                size: item.size,
            },
        );

        match upload_one(&app, &client, &bucket, item, index, &key, rx.clone()).await {
            Ok(()) => {
                let _ = app.emit(
                    "upload://file-done",
                    FileDoneEvent {
                        index,
                        path: item.path.clone(),
                    },
                );
                uploaded_count += 1;
            }
            Err(e) if e == CANCELLED => {
                let _ = app.emit(
                    "upload://file-error",
                    FileErrorEvent {
                        index,
                        path: item.path.clone(),
                        error: CANCELLED.to_string(),
                    },
                );
                cancelled = true;
                break;
            }
            Err(e) => {
                let _ = app.emit(
                    "upload://file-error",
                    FileErrorEvent {
                        index,
                        path: item.path.clone(),
                        error: e,
                    },
                );
                failed_count += 1;
            }
        }
    }

    *state.cancel_tx.lock().unwrap() = None;

    let done = UploadDoneEvent {
        uploaded: uploaded_count,
        failed: failed_count,
        cancelled,
    };
    let _ = app.emit("upload://done", done.clone());
    Ok(done)
}

#[tauri::command]
fn cancel_upload(state: State<'_, UploadState>) {
    if let Some(tx) = state.cancel_tx.lock().unwrap().as_ref() {
        let _ = tx.send(true);
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(UploadState {
            cancel_tx: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            select_folder,
            scan_folder,
            upload_files,
            cancel_upload
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
