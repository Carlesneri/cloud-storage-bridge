import { useEffect, useRef, useMemo, useState } from "react";
import logo from "./assets/app-logo.png";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import "./App.css";

type MediaKind = "image" | "video" | "audio";

interface MediaFile {
  path: string;
  relative: string;
  size: number;
  mtime: number;
  kind: MediaKind;
}

interface HistoryEntry {
  size: number;
  mtime: number;
  key: string;
  uploaded_at: number;
}

type History = Record<string, HistoryEntry>;

interface SkipList {
  [path: string]: number;
}

interface R2Config {
  account_id: string;
  access_key_id: string;
  secret_access_key: string;
  bucket: string;
  prefix: string;
  endpoint: string;
}

interface UploadItem {
  path: string;
  relative: string;
  size: number;
}interface FileStartEvent {
  index: number;
  path: string;
  key: string;
  size: number;
}

interface FileProgressEvent {
  index: number;
  path: string;
  uploaded: number;
}

interface FileDoneEvent {
  index: number;
  path: string;
}

interface FileErrorEvent {
  index: number;
  path: string;
  error: string;
}

interface UploadDoneEvent {
  uploaded: number;
  failed: number;
  cancelled: boolean;
}

interface FileStatus {
  state: "pending" | "active" | "done" | "error";
  uploaded: number;
  error?: string;
}

const DEFAULT_CONFIG: R2Config = {
  account_id: "",
  access_key_id: "",
  secret_access_key: "",
  bucket: "",
  prefix: "",
  endpoint: "",
};

const KIND_LABEL: Record<MediaKind, string> = {
  image: "Image",
  video: "Video",
  audio: "Audio",
};

function loadSavedConfig(): R2Config {
  return { ...DEFAULT_CONFIG };
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
  return `${(bytes / 1024 ** i).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function KindBadge({ kind }: { kind: MediaKind }) {
  return <span className={`badge badge-${kind}`}>{KIND_LABEL[kind][0]}</span>;
}

function App() {
  const [config, setConfig] = useState<R2Config>(loadSavedConfig);
  const configLoaded = useRef(false);
  const [folder, setFolder] = useState<string | null>(null);
  const [files, setFiles] = useState<MediaFile[]>([]);
  const [excluded, setExcluded] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState<"all" | MediaKind>("all");
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);

  const [uploading, setUploading] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const [statuses, setStatuses] = useState<Record<string, FileStatus>>({});
  const [history, setHistory] = useState<History>({});
  const [skipList, setSkipList] = useState<SkipList>({});
  const [activeKey, setActiveKey] = useState<string | null>(null);
  const [result, setResult] = useState<UploadDoneEvent | null>(null);
  const [commandError, setCommandError] = useState<string | null>(null);

  useEffect(() => {
    invoke<R2Config | null>("load_config")
      .then((saved) => {
        if (saved) setConfig({ ...DEFAULT_CONFIG, ...saved });
      })
      .catch(() => {})
      .finally(() => {
        configLoaded.current = true;
      });
    invoke<History>("get_history")
      .then((h) => setHistory(h ?? {}))
      .catch(() => {});
    invoke<SkipList>("get_skip_list")
      .then((s) => setSkipList(s ?? {}))
      .catch(() => {});
  }, []);

  // Persist the non-secret settings whenever they change.
  useEffect(() => {
    if (!configLoaded.current) return;
    invoke("save_config", { config }).catch(() => {});
  }, [config]);

  // Persist the secret to the OS keychain, but only when the user actually
  // types one — leaving the field blank means "use the stored one". Writes are
  // debounced so the keychain prompt appears at most once per edit.
  useEffect(() => {
    if (!configLoaded.current) return;
    const secret = config.secret_access_key.trim();
    if (secret.length === 0) return; // never overwrite stored with empty
    const timer = setTimeout(() => {
      invoke("save_secret", { secret: config.secret_access_key }).catch(() => {});
    }, 700);
    return () => clearTimeout(timer);
  }, [config.secret_access_key]);

  const counts = useMemo(() => {
    const c = { all: 0, image: 0, video: 0, audio: 0 } as Record<"all" | MediaKind, number>;
    for (const f of files) c[f.kind] += 1;
    c.all = files.length;
    return c;
  }, [files]);

  const selected = useMemo(
    () => files.filter((f) => !excluded.has(f.path)),
    [files, excluded],
  );
  const selectedBytes = useMemo(
    () => selected.reduce((sum, f) => sum + f.size, 0),
    [selected],
  );

  const visible = useMemo(
    () => (filter === "all" ? files : files.filter((f) => f.kind === filter)),
    [files, filter],
  );

  const uploadedBytes = useMemo(() => {
    let sum = 0;
    for (const f of selected) {
      const st = statuses[f.path];
      if (st) sum += Math.min(st.uploaded, f.size);
    }
    return sum;
  }, [selected, statuses]);

  const configValid =
    config.bucket.trim().length > 0 &&
    config.access_key_id.trim().length > 0 &&
    (config.account_id.trim().length > 0 || config.endpoint.trim().length > 0);

  const hasCredentials = Object.values(config).some((v) => v.trim().length > 0);

  async function pickFolder() {
    setScanError(null);
    setResult(null);
    setStatuses({});
    setActiveKey(null);
    try {
      const path = await invoke<string | null>("select_folder");
      if (!path) return;
      setFolder(path);
      setScanning(true);
      setFilter("all");
      const [media, hist, skip] = await Promise.all([
        invoke<MediaFile[]>("scan_folder", { path }),
        invoke<History>("get_history").catch<History>(() => ({})),
        invoke<SkipList>("get_skip_list").catch<SkipList>(() => ({})),
      ]);
      setHistory(hist ?? {});
      setSkipList(skip ?? {});
      const initialExcluded = new Set<string>();
      for (const f of media) {
        const h = hist[f.path];
        if (h && h.size === f.size && h.mtime === f.mtime) {
          initialExcluded.add(f.path);
        } else if (skip[f.path] !== undefined) {
          initialExcluded.add(f.path);
        }
      }
      setExcluded(initialExcluded);
      setFiles(media);
      if (media.length === 0) {
        setScanError("No media files found in this folder.");
      }
    } catch (e) {
      setFolder(null);
      setFiles([]);
      setScanError(String(e));
    } finally {
      setScanning(false);
    }
  }

  function toggleFile(path: string) {
    const wasSkipped = skipList[path] !== undefined;
    const becomingIncluded = excluded.has(path);
    setExcluded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
    if (wasSkipped && becomingIncluded) {
      const next = { ...skipList };
      delete next[path];
      setSkipList(next);
      invoke("remove_skip", { path: path }).catch(() => {});
    }
  }

  function skipFile(path: string) {
    setExcluded((prev) => {
      const next = new Set(prev);
      next.add(path);
      return next;
    });
    const now = Math.floor(Date.now() / 1000);
    setSkipList((prev) => ({ ...prev, [path]: now }));
    invoke("add_skip", { path: path }).catch(() => {});
  }

  function toggleVisible(check: boolean) {
    setExcluded((prev) => {
      const next = new Set(prev);
      for (const f of visible) {
        if (!check) next.add(f.path);
        else next.delete(f.path);
      }
      return next;
    });
  }

  async function cancelUpload() {
    if (cancelling) return;
    setCancelling(true);
    try {
      await invoke("cancel_upload");
    } catch {
      // ignore
    }
  }

  async function startUpload() {
    const items: UploadItem[] = selected.map((f) => ({
      path: f.path,
      relative: f.relative,
      size: f.size,
    }));
    if (items.length === 0 || uploading) return;

    // The secret is stored in the OS keychain, not the React state — fetch it
    // lazily so a keychain prompt never appears on app launch.
    let secret = config.secret_access_key.trim();
    if (secret.length === 0) {
      const stored = await invoke<string | null>("load_secret").catch<null>(() => null);
      secret = (stored ?? "").trim();
    }
    if (secret.length === 0) {
      setCommandError("Enter your R2 Secret Access Key to upload.");
      return;
    }

    const initial: Record<string, FileStatus> = {};
    for (const f of selected) initial[f.path] = { state: "pending", uploaded: 0 };
    setStatuses(initial);
    setResult(null);
    setCommandError(null);
    setActiveKey(null);
    setCancelling(false);
    setUploading(true);

    const metaByPath = new Map(selected.map((f) => [f.path, f]));
    const keyByPath = new Map<string, string>();

    const unlistens: UnlistenFn[] = await Promise.all([
      listen<FileStartEvent>("upload://file-start", (e) => {
        keyByPath.set(e.payload.path, e.payload.key);
        setStatuses((prev) => ({
          ...prev,
          [e.payload.path]: { state: "active", uploaded: 0 },
        }));
        setActiveKey(e.payload.key);
      }),
      listen<FileProgressEvent>("upload://progress", (e) => {
        setStatuses((prev) => {
          const cur = prev[e.payload.path];
          if (!cur) return prev;
          return {
            ...prev,
            [e.payload.path]: { ...cur, uploaded: e.payload.uploaded },
          };
        });
      }),
      listen<FileDoneEvent>("upload://file-done", (e) => {
        const meta = metaByPath.get(e.payload.path);
        setStatuses((prev) => {
          const cur = prev[e.payload.path];
          if (!cur) return prev;
          return { ...prev, [e.payload.path]: { state: "done", uploaded: cur.uploaded } };
        });
        setHistory((prev) => ({
          ...prev,
          [e.payload.path]: {
            size: meta?.size ?? 0,
            mtime: meta?.mtime ?? 0,
            key: keyByPath.get(e.payload.path) ?? "",
            uploaded_at: Math.floor(Date.now() / 1000),
          },
        }));
      }),
      listen<FileErrorEvent>("upload://file-error", (e) => {
        setStatuses((prev) => {
          const cur = prev[e.payload.path];
          const error =
            e.payload.error === "cancelled" ? "Cancelled" : e.payload.error;
          return {
            ...prev,
            [e.payload.path]: {
              state: "error",
              uploaded: cur?.uploaded ?? 0,
              error,
            },
          };
        });
      }),
    ]);

    try {
      const done = await invoke<UploadDoneEvent>("upload_files", {
        config: { ...config, secret_access_key: secret },
        items,
        root: folder,
      });
      setResult(done);
    } catch (e) {
      setCommandError(String(e));
    } finally {
      for (const u of unlistens) u();
      setUploading(false);
      setActiveKey(null);
      setCancelling(false);
    }
  }

  const overallPct =
    selectedBytes > 0 ? Math.round((uploadedBytes / selectedBytes) * 100) : 0;

  return (
    <main className="app">
      <header className="header">
        <div className="header-title">
          <img src={logo} className="logo" alt="Cloud Storage Bridge" />
          <div>
            <h1>Cloud Storage Bridge</h1>
            <p>Upload a folder's media to a Cloudflare R2 bucket</p>
          </div>
        </div>
      </header>

      <section className="card">
        <div className="card-header">
          <h2>Destination</h2>
          {hasCredentials && (
            <button
              className="btn ghost btn-clear-creds"
              onClick={() => {
                setConfig({ ...DEFAULT_CONFIG });
                invoke("save_secret", { secret: "" }).catch(() => {});
              }}
              disabled={uploading}
              title="Clear all fields and remove the saved secret from the OS keychain"
            >
              Clear credentials
            </button>
          )}
        </div>
        <div className="config-grid">
          <label>
            Account ID
            <input
              value={config.account_id}
              onChange={(e) => setConfig({ ...config, account_id: e.target.value })}
              placeholder="e.g. 1a2b3c4d5e6f..."
              spellCheck={false}
            />
          </label>
          <label>
            Bucket
            <input
              value={config.bucket}
              onChange={(e) => setConfig({ ...config, bucket: e.target.value })}
              placeholder="my-bucket"
              spellCheck={false}
            />
          </label>
          <label>
            Access Key ID
            <input
              value={config.access_key_id}
              onChange={(e) => setConfig({ ...config, access_key_id: e.target.value })}
              placeholder="R2 access key id"
              spellCheck={false}
              autoComplete="off"
            />
          </label>
          <label>
            Secret Access Key <span className="optional">stored in keychain</span>
            <input
              type="password"
              value={config.secret_access_key}
              onChange={(e) =>
                setConfig({ ...config, secret_access_key: e.target.value })
              }
              placeholder="Leave blank to use the saved key"
              autoComplete="off"
            />
          </label>
          <label>
            Key prefix <span className="optional">optional</span>
            <input
              value={config.prefix}
              onChange={(e) => setConfig({ ...config, prefix: e.target.value })}
              placeholder="photos/2026"
              spellCheck={false}
            />
          </label>
          <label>
            S3 endpoint <span className="optional">advanced</span>
            <input
              value={config.endpoint}
              onChange={(e) => setConfig({ ...config, endpoint: e.target.value })}
              placeholder="auto: https://<account>.r2.cloudflarestorage.com"
              spellCheck={false}
            />
          </label>
        </div>
        {!configValid && (
          <p className="hint">
            Fill in your R2 account ID, credentials and bucket to enable uploads.
          </p>
        )}
      </section>

      <section className="card">
        <div className="folder-row">
          <button className="btn secondary" onClick={pickFolder} disabled={uploading || scanning}>
            {scanning ? "Scanning..." : folder ? "Change Folder..." : "Choose Folder..."}
          </button>
          {folder && (
            <span className="folder-path" title={folder}>
              {folder}
            </span>
          )}
        </div>

        {files.length > 0 && (
          <>
            <div className="toolbar">
              <div className="chips">
                {(["all", "image", "video", "audio"] as const).map((k) => (
                  <button
                    key={k}
                    className={`chip ${filter === k ? "chip-active" : ""}`}
                    onClick={() => setFilter(k)}
                  >
                    {k === "all" ? "All" : `${KIND_LABEL[k]}s`}
                    <span className="chip-count">{counts[k]}</span>
                  </button>
                ))}
              </div>
              <div className="toolbar-actions">
                <button className="btn ghost" onClick={() => toggleVisible(true)}>
                  Select all
                </button>
                <button className="btn ghost" onClick={() => toggleVisible(false)}>
                  Deselect all
                </button>
              </div>
            </div>

            <ul className="file-list">
              {visible.map((f) => {
                const st = statuses[f.path];
                const included = !excluded.has(f.path);
                const hist = history[f.path];
                const upToDate =
                  hist !== undefined && hist.size === f.size && hist.mtime === f.mtime;
                const modified = hist !== undefined && !upToDate;
                const skipped = skipList[f.path] !== undefined;
                return (
                  <li
                    key={f.path}
                    className={`file-row ${st?.state === "error" ? "file-error" : ""} ${skipped ? "file-skipped" : ""}`}
                  >
                    <input
                      type="checkbox"
                      checked={included}
                      onChange={() => toggleFile(f.path)}
                      disabled={uploading}
                    />
                    <KindBadge kind={f.kind} />
                    <span className="file-name" title={f.path}>
                      {f.relative}
                    </span>
                    {upToDate && (
                      <span
                        className="tag tag-uploaded"
                        title={`Uploaded ${new Date(hist.uploaded_at * 1000).toLocaleString()}`}
                      >
                        Uploaded
                      </span>
                    )}
                    {modified && (
                      <span
                        className="tag tag-modified"
                        title="File changed since last upload"
                      >
                        Modified
                      </span>
                    )}
                    {skipped && (
                      <span
                        className="tag tag-skipped"
                        title={`Skipped ${new Date(skipList[f.path] * 1000).toLocaleString()}`}
                      >
                        Skipped
                      </span>
                    )}
                    {uploading && st ? (
                      <span className={`status status-${st.state}`}>
                        {st.state === "active" &&
                          `${Math.floor(
                            (Math.min(st.uploaded, f.size) / Math.max(f.size, 1)) * 100,
                          )}%`}
                        {st.state === "pending" && "Waiting"}
                        {st.state === "done" && "Done"}
                        {st.state === "error" && (st.error ?? "Error")}
                      </span>
                    ) : null}
                    {!uploading && !upToDate && (
                      <button
                        className="btn ghost btn-skip"
                        onClick={() => skipFile(f.path)}
                        disabled={skipped}
                        title="Don't upload this file; remember the choice"
                      >
                        Skip
                      </button>
                    )}
                    <span className="file-size">{formatBytes(f.size)}</span>
                  </li>
                );
              })}
            </ul>
          </>
        )}
        {scanError && <p className="hint">{scanError}</p>}
      </section>

      {files.length > 0 && (
        <footer className="upload-bar">
          <div className="upload-meta">
            <strong>
              {selected.length} / {files.length} files
            </strong>
            <span>{formatBytes(selectedBytes)}</span>
            {activeKey && uploading && <span className="active-key">Uploading: {activeKey}</span>}
          </div>
          <div className={`progress ${uploading ? "" : "idle"}`}>
            <div className="progress-fill" style={{ width: `${overallPct}%` }} />
          </div>
          <div className="upload-actions">
            {uploading ? (
              <button className="btn danger" onClick={cancelUpload} disabled={cancelling}>
                {cancelling ? "Cancelling..." : "Cancel"}
              </button>
            ) : (
              <button
                className="btn primary"
                onClick={startUpload}
                disabled={!configValid || selected.length === 0}
              >
                Upload to R2
              </button>
            )}
            <span className="pct">{overallPct}%</span>
          </div>
          {result && (
            <p className={`summary ${result.failed > 0 ? "warn" : "ok"}`}>
              {result.cancelled
                ? `Cancelled. ${result.uploaded} uploaded, ${result.failed} failed.`
                : result.failed > 0
                  ? `Finished with errors. ${result.uploaded} uploaded, ${result.failed} failed.`
                  : `Done. ${result.uploaded} file${result.uploaded === 1 ? "" : "s"} uploaded.`}
            </p>
          )}
          {commandError && <p className="summary warn">{commandError}</p>}
        </footer>
      )}
    </main>
  );
}

export default App;
