use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aws_sdk_s3::{
    config::{Credentials, Region},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
    Client,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
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
    mtime: u64,
    kind: MediaKind,
}

#[derive(Clone, Serialize, Deserialize)]
struct HistoryEntry {
    size: u64,
    mtime: u64,
    key: String,
    uploaded_at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
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
        let metadata = entry.metadata().map_err(|e| e.to_string())?;
        let size = metadata.len();
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
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
            mtime,
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

/// Render an error together with its whole source chain, so connector-level
/// failures (e.g. "dispatch failure") show the actual underlying cause.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        out.push_str(&format!(": {s}"));
        source = s.source();
    }
    out
}

async fn wait_cancelled(rx: &mut tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    while rx.changed().await.is_ok() {
        if *rx.borrow_and_update() {
            return;
        }
    }
}

/// Run `fut`, aborting it as soon as a cancellation is signalled.
async fn cancelable<T, F>(
    rx: &mut tokio::sync::watch::Receiver<bool>,
    fut: F,
) -> Result<T, String>
where
    F: std::future::Future<Output = Result<T, String>>,
{
    tokio::select! {
        _ = wait_cancelled(rx) => Err(CANCELLED.to_string()),
        res = fut => res,
    }
}

/// HTTP/1.1-only S3 HTTP client (Cloudflare R2 can reset HTTP/2 connections
/// mid-session, which the AWS SDK surfaces as opaque "dispatch failure"
/// errors). Restricting ALPN to http/1.1 avoids that class of failures.
#[derive(Clone)]
struct H1OnlyConnector {
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        aws_smithy_types::body::SdkBody,
    >,
}

impl std::fmt::Debug for H1OnlyConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("H1OnlyConnector")
    }
}

impl aws_smithy_runtime_api::client::http::HttpConnector for H1OnlyConnector {
    fn call(
        &self,
        request: aws_smithy_runtime_api::client::orchestrator::HttpRequest,
    ) -> aws_smithy_runtime_api::client::http::HttpConnectorFuture {
        let request = match request.try_into_http1x() {
            Ok(r) => r,
            Err(err) => {
                return aws_smithy_runtime_api::client::http::HttpConnectorFuture::ready(Err(
                    aws_smithy_runtime_api::client::result::ConnectorError::user(err.into()),
                ));
            }
        };
        let client = self.client.clone();
        let fut = client.request(request);
        aws_smithy_runtime_api::client::http::HttpConnectorFuture::new(async move {
            let response = fut
                .await
                .map_err(|e| {
                    aws_smithy_runtime_api::client::result::ConnectorError::io(Box::new(e))
                })?
                .map(aws_smithy_types::body::SdkBody::from_body_1_x);
            match aws_smithy_runtime_api::client::orchestrator::HttpResponse::try_from(response) {
                Ok(response) => Ok(response),
                Err(err) => Err(aws_smithy_runtime_api::client::result::ConnectorError::other(
                    err.into(),
                    None,
                )),
            }
        })
    }
}

fn build_http_client() -> aws_smithy_runtime_api::client::http::SharedHttpClient {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        panic!("no TLS root certificates found in the OS certificate store");
    }

    let tls_config = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .expect("supported TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();

    let mut tcp = hyper_util::client::legacy::connect::HttpConnector::new();
    tcp.enforce_http(false);
    tcp.set_connect_timeout(Some(Duration::from_secs(30)));

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls_config)
        .https_or_http()
        .enable_http1()
        .wrap_connector(tcp);

    let hyper_client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(https);

    let connector: aws_smithy_runtime_api::client::http::SharedHttpConnector =
        aws_smithy_runtime_api::client::http::SharedHttpConnector::new(H1OnlyConnector {
            client: hyper_client,
        });
    aws_smithy_runtime_api::client::http::http_client_fn(move |_, _| connector.clone())
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
        cancelable(
            &mut cancel_rx,
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
                        .map_err(|e| error_chain(&e))
                }
            }),
        )
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

    let mpu = cancelable(
        &mut cancel_rx,
        with_retry(|| async {
            client
                .create_multipart_upload()
                .bucket(bucket)
                .key(key)
                .content_type(&content_type)
                .send()
                .await
                .map_err(|e| error_chain(&e))
        }),
    )
    .await
    .map_err(|e| {
        if e == CANCELLED {
            e
        } else {
            format!("failed to start multipart upload: {e}")
        }
    })?;

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
            let result = cancelable(
                &mut cancel_rx,
                with_retry(|| {
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
                            .map_err(|e| error_chain(&e))
                    }
                }),
            )
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

        let resp = cancelable(
            cancel_rx,
            with_retry(|| {
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
                        .map_err(|e| error_chain(&e))
                }
            }),
        )
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

/// Make sure a user-provided endpoint is an absolute URL: hyper rejects
/// scheme-less URIs with "invalid URL, scheme is not http".
fn normalize_endpoint(raw: &str) -> String {
    let ep = raw.trim().trim_end_matches('/');
    if ep.starts_with("http://") || ep.starts_with("https://") {
        ep.to_string()
    } else {
        format!("https://{ep}")
    }
}

#[tauri::command]
async fn upload_files(
    app: AppHandle,
    state: State<'_, UploadState>,
    config: R2Config,
    items: Vec<UploadItem>,
    root: String,
) -> Result<UploadDoneEvent, String> {
    if items.is_empty() {
        return Err("No files selected".into());
    }
    let root_canonical = std::fs::canonicalize(&root)
        .map_err(|e| format!("cannot resolve folder `{root}`: {e}"))?;
    if !root_canonical.is_dir() {
        return Err(format!("`{root}` is not a directory"));
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
        if !account.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(
                "Invalid R2 account id: paste only the account id (e.g. 1a2b3c4d5e6f), not a URL"
                    .into(),
            );
        }
        format!("https://{account}.r2.cloudflarestorage.com")
    } else {
        normalize_endpoint(&config.endpoint)
    };

    // R2 can reset HTTP/2 connections mid-session (surfacing as opaque
    // "dispatch failure" errors); an HTTP/1.1-only client avoids that.
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .http_client(build_http_client())
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
    let mut history = read_history(&app);

    for (index, item) in items.iter().enumerate() {
        if *rx.borrow_and_update() {
            cancelled = true;
            break;
        }

        // Only upload files that live inside the folder the user picked.
        let in_root = std::fs::canonicalize(&item.path)
            .map(|p| p.starts_with(&root_canonical))
            .unwrap_or(false);
        if !in_root {
            let _ = app.emit(
                "upload://file-error",
                FileErrorEvent {
                    index,
                    path: item.path.clone(),
                    error: "path is outside the selected folder".to_string(),
                },
            );
            failed_count += 1;
            continue;
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
                let mtime = tokio::fs::metadata(&item.path)
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                history.insert(
                    item.path.clone(),
                    HistoryEntry {
                        size: item.size,
                        mtime,
                        key: key.clone(),
                        uploaded_at: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0),
                    },
                );
                let _ = write_history(&app, &history);
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

fn config_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir unavailable: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    Ok(dir.join("r2-config.json"))
}

fn history_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir unavailable: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    Ok(dir.join("upload-history.json"))
}

fn read_history(app: &AppHandle) -> HashMap<String, HistoryEntry> {
    history_file_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn write_history(app: &AppHandle, history: &HashMap<String, HistoryEntry>) -> Result<(), String> {
    let path = history_file_path(app)?;
    let json = serde_json::to_string_pretty(history).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("write history failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[tauri::command]
fn get_history(app: AppHandle) -> Result<HashMap<String, HistoryEntry>, String> {
    Ok(read_history(&app))
}

#[tauri::command]
fn clear_history(app: AppHandle) -> Result<(), String> {
    write_history(&app, &HashMap::new())
}

fn skip_list_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|e| format!("config dir unavailable: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create config dir failed: {e}"))?;
    Ok(dir.join("skip-list.json"))
}

fn read_skip_list(app: &AppHandle) -> HashMap<String, u64> {
    skip_list_file_path(app)
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn write_skip_list(
    app: &AppHandle,
    skip_list: &HashMap<String, u64>,
) -> Result<(), String> {
    let path = skip_list_file_path(app)?;
    let json = serde_json::to_string_pretty(skip_list).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("write skip list failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[tauri::command]
fn get_skip_list(app: AppHandle) -> Result<HashMap<String, u64>, String> {
    Ok(read_skip_list(&app))
}

#[tauri::command]
fn add_skip(app: AppHandle, path: String) -> Result<(), String> {
    let mut skip_list = read_skip_list(&app);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    skip_list.insert(path, now);
    write_skip_list(&app, &skip_list)
}

#[tauri::command]
fn remove_skip(app: AppHandle, path: String) -> Result<(), String> {
    let mut skip_list = read_skip_list(&app);
    skip_list.remove(&path);
    write_skip_list(&app, &skip_list)
}

const KEYCHAIN_SERVICE: &str = "Cloud Storage Bridge";
const KEYCHAIN_SECRET_KEY: &str = "R2 secret access key";
const LEGACY_KEYCHAIN_SERVICE: &str = "cloud-storage-bridge";
const LEGACY_KEYCHAIN_SECRET_KEY: &str = "r2-secret-access-key";

fn keychain_entry() -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_SECRET_KEY)
        .map_err(|e| format!("keychain unavailable: {e}"))
}

fn write_config_file(app: &AppHandle, config: &R2Config) -> Result<(), String> {
    let path = config_file_path(app)?;
    let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| format!("write config failed: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[tauri::command]
fn save_config(app: AppHandle, mut config: R2Config) -> Result<(), String> {
    // Persist only the non-secret settings here. The secret is written to the
    // OS keychain via `save_secret`, only when it actually changes, so we don't
    // spam the macOS keychain access prompt on every keystroke in any field.
    config.secret_access_key = String::new();
    write_config_file(&app, &config)
}

#[tauri::command]
fn save_secret(secret: String) -> Result<(), String> {
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        if let Ok(entry) = keychain_entry() {
            let _ = entry.delete_credential();
        }
        Ok(())
    } else {
        keychain_entry()?
            .set_password(trimmed)
            .map_err(|e| format!("keychain write failed: {e}"))
    }
}

#[tauri::command]
fn load_config(app: AppHandle) -> Result<Option<R2Config>, String> {
    // The keychain is intentionally NOT read here, so launching the app never
    // triggers a keychain access prompt. The secret is fetched lazily via
    // `load_secret` (typically only when an upload is started).
    let path = config_file_path(&app)?;
    let mut config: Option<R2Config> = match std::fs::read_to_string(&path) {
        Ok(json) => Some(serde_json::from_str(&json).map_err(|e| e.to_string())?),
        Err(_) => None,
    };
    if let Some(cfg) = config.as_mut() {
        cfg.secret_access_key = String::new();
    }
    Ok(config)
}

#[tauri::command]
fn load_secret() -> Result<Option<String>, String> {
    match keychain_entry()?.get_password() {
        Ok(s) if !s.is_empty() => Ok(Some(s)),
        _ => {
            // One-time migration from the old keychain entry (and from any
            // legacy plaintext secret baked into the config file).
            if let Ok(entry) = keyring::Entry::new(LEGACY_KEYCHAIN_SERVICE, LEGACY_KEYCHAIN_SECRET_KEY) {
                if let Ok(s) = entry.get_password() {
                    if !s.is_empty() {
                        let _ = keychain_entry()?.set_password(&s);
                        let _ = entry.delete_credential();
                        return Ok(Some(s));
                    }
                }
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_endpoints() {
        assert_eq!(
            normalize_endpoint("abc.r2.cloudflarestorage.com"),
            "https://abc.r2.cloudflarestorage.com"
        );
        assert_eq!(
            normalize_endpoint(" https://abc.r2.cloudflarestorage.com/ "),
            "https://abc.r2.cloudflarestorage.com"
        );
        assert_eq!(
            normalize_endpoint("http://localhost:9000/"),
            "http://localhost:9000"
        );
    }

    /// Capture the raw request bytes a client sends, one connection at a time.
    async fn capture_request(listener: &tokio::net::TcpListener) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            tokio::select! {
                r = socket.read(&mut chunk) => {
                    match r {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if find_headers_end(&buf).is_some() {
                                let headers = String::from_utf8_lossy(&buf).to_string();
                                let empty_body = header_value(
                                    &headers,
                                    "content-length",
                                )
                                .map(|v| v.trim() == "0")
                                .unwrap_or(true);
                                if empty_body {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                break;
                            }
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(5)) => break,
            }
        }
        let body = "<CreateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key><UploadId>x</UploadId></CreateMultipartUploadResult>";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn find_headers_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
    }

    fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
        headers.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case(name) {
                Some(v.trim())
            } else {
                None
            }
        })
    }

    fn make_client(endpoint: &str, custom: bool) -> Client {
        let mut b = aws_sdk_s3::Config::builder()
            .behavior_version_latest()
            .credentials_provider(Credentials::new(
                "AKIDEXAMPLE",
                "wJalrXUtnFEMI",
                None,
                None,
                "test",
            ))
            .endpoint_url(endpoint)
            .region(Region::new("auto"))
            .force_path_style(true);
        if custom {
            b = b.http_client(build_http_client());
        }
        Client::from_conf(b.build())
    }

    async fn run_one(
        listener: &tokio::net::TcpListener,
        endpoint: &str,
        custom: bool,
    ) -> String {
        let endpoint = endpoint.to_string();
        let req = tokio::spawn(async move {
            let _ = make_client(&endpoint, custom)
                .create_multipart_upload()
                .bucket("b")
                .key("k")
                .send()
                .await;
        });
        let raw = capture_request(listener).await;
        let _ = req.await;
        raw
    }

    /// The bytes sent by the custom HTTP/1.1 connector must be structurally
    /// identical to what the default SDK client sends for the same operation
    /// (else the SigV4 signature won't match).
    #[tokio::test]
    async fn h1_connector_bytes_match_default_client() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("http://{addr}");

        let default_raw = run_one(&listener, &endpoint, false).await;
        let custom_raw = run_one(&listener, &endpoint, true).await;

        let strip = |req: &str| -> Vec<String> {
            let (head, body) = req.split_once("\r\n\r\n").unwrap_or((req, ""));
            let mut lines: Vec<String> = head
                .lines()
                .map(|l| {
                    let lower = l.to_ascii_lowercase();
                    if lower.starts_with("authorization:")
                        || lower.starts_with("x-amz-date:")
                        || lower.starts_with("user-agent:")
                        || lower.starts_with("x-amz-user-agent:")
                        || lower.starts_with("amz-sdk-invocation-id:")
                    {
                        lower.split(':').next().unwrap().to_string()
                    } else {
                        lower
                    }
                })
                .collect();
            lines.sort();
            if !body.is_empty() {
                lines.push(format!("body-len:{}", body.len()));
            }
            lines
        };

        println!("--- default ---\n{default_raw}\n--- custom ---\n{custom_raw}");
        assert_eq!(
            strip(&default_raw),
            strip(&custom_raw),
            "custom connector sends structurally different request bytes"
        );
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
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // If an upload is in progress, cancel it first and defer
                // closing the window until it has actually stopped, so any
                // in-flight request is properly aborted (and multipart uploads
                // are cleaned up on the server) rather than being dropped.
                let state = window.state::<UploadState>();
                let tx = state.cancel_tx.lock().unwrap().clone();
                if let Some(tx) = tx {
                    api.prevent_close();
                    let _ = tx.send(true);
                    let win = window.clone();
                    tauri::async_runtime::spawn(async move {
                        loop {
                            let still_running = win
                                .state::<UploadState>()
                                .cancel_tx
                                .lock()
                                .map(|g| g.is_some())
                                .unwrap_or(false);
                            if !still_running {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        let _ = win.close();
                    });
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            select_folder,
            scan_folder,
            upload_files,
            cancel_upload,
            save_config,
            load_config,
            save_secret,
            load_secret,
            get_history,
            clear_history,
            get_skip_list,
            add_skip,
            remove_skip
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
