import { useEffect, useRef, useMemo, useState } from "react"
import logo from "./assets/app-logo.png"
import { invoke } from "@tauri-apps/api/core"
import { listen, type UnlistenFn } from "@tauri-apps/api/event"
import "./App.css"

type MediaKind = "image" | "video" | "audio"

interface MediaFile {
  path: string
  relative: string
  size: number
  mtime: number
  kind: MediaKind
}

interface HistoryEntry {
  size: number
  mtime: number
  key: string
  uploaded_at: number
}

type History = Record<string, HistoryEntry>

interface SkipList {
  [path: string]: number
}

interface R2Config {
  account_id: string
  access_key_id: string
  secret_access_key: string
  bucket: string
  prefix: string
  endpoint: string
}

interface UploadItem {
  path: string
  relative: string
  size: number
  transcode: boolean
}

interface FileStartEvent {
  index: number
  path: string
  key: string
  size: number
}

interface FileProgressEvent {
  index: number
  path: string
  uploaded: number
}

interface FileDoneEvent {
  index: number
  path: string
  warning?: string
}

interface FileErrorEvent {
  index: number
  path: string
  error: string
}

interface UploadDoneEvent {
  uploaded: number
  failed: number
  cancelled: boolean
}

interface PrepareStartEvent {
  index: number
  path: string
  action: string
  duration: number
}

interface PrepareProgressEvent {
  index: number
  path: string
  seconds: number
  duration: number
}

interface FileStatus {
  state: "pending" | "preparing" | "active" | "done" | "error"
  uploaded: number
  action?: string
  preparePct?: number
  warning?: string
  error?: string
}

const DEFAULT_CONFIG: R2Config = {
  account_id: "",
  access_key_id: "",
  secret_access_key: "",
  bucket: "",
  prefix: "",
  endpoint: "",
}

const KIND_LABEL: Record<MediaKind, string> = {
  image: "Image",
  video: "Video",
  audio: "Audio",
}

function formatBytes(bytes: number): string {
  if (bytes === 0) return "0 B"
  const units = ["B", "KB", "MB", "GB", "TB"]
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  return `${(bytes / 1024 ** i).toFixed(i === 0 ? 0 : 1)} ${units[i]}`
}

/** Immutable Set update: copy `prev`, apply `mutate`, return the copy. */
function updateSet(prev: Set<string>, mutate: (next: Set<string>) => void): Set<string> {
  const next = new Set(prev)
  mutate(next)
  return next
}

/** Status pill for a file row; null means "render nothing (warning tag only)". */
function statusView(
  st: FileStatus | undefined,
  size: number,
  live: boolean,
): { cls: string; text: string } | null {
  if (st && (live || st.state === "done" || st.state === "error")) {
    switch (st.state) {
      case "done":
        return { cls: "status-done", text: "Uploaded" }
      case "error":
        return { cls: "status-error", text: st.error ?? "Error" }
      case "pending":
        return { cls: "status-pending", text: "Waiting" }
      case "preparing":
        return {
          cls: "status-preparing",
          text: `${st.action ?? "preparing"}${st.preparePct ? ` ${st.preparePct}%` : ""}`,
        }
      case "active": {
        const pct = Math.floor((Math.min(st.uploaded, size) / Math.max(size, 1)) * 100)
        return { cls: "status-active", text: `${pct}%` }
      }
    }
  }
  if (!st?.warning) return { cls: "status-ready", text: "Ready" }
  return null
}

/** Labeled switch used in the upload footer. */
function Toggle(props: {
  label: string
  title: string
  checked: boolean
  disabled?: boolean
  onChange?: (value: boolean) => void
}) {
  return (
    <label className="delete-toggle" title={props.title}>
      {props.label}
      <input
        type="checkbox"
        className="switch"
        role="switch"
        checked={props.checked}
        disabled={props.disabled}
        onChange={(e) => props.onChange?.(e.target.checked)}
      />
    </label>
  )
}

const DELETE_KEY = "delete-after-upload"

function loadDeletePref(): boolean {
  return localStorage.getItem(DELETE_KEY) === "1"
}

function App() {
  const [config, setConfig] = useState<R2Config>(() => ({ ...DEFAULT_CONFIG }))
  const configLoaded = useRef(false)
  const [folder, setFolder] = useState<string | null>(null)
  const [files, setFiles] = useState<MediaFile[]>([])
  const [excluded, setExcluded] = useState<Set<string>>(new Set())
  const [filter, setFilter] = useState<"all" | MediaKind>("all")
  const [scanning, setScanning] = useState(false)
  const [scanError, setScanError] = useState<string | null>(null)

  const [uploading, setUploading] = useState(false)
  const [cancelling, setCancelling] = useState(false)
  const [autoActive, setAutoActive] = useState(false)
  const autoActiveRef = useRef(false)
  const [destOpen, setDestOpen] = useState(false)
  const [transcodeSet, setTranscodeSet] = useState<Set<string>>(new Set())
  const [deleteAfterUpload, setDeleteAfterUpload] = useState(loadDeletePref)
  const [autoUpload, setAutoUpload] = useState(false)
  const autoUploadRef = useRef(false)
  const secretRef = useRef<string | null>(null)
  const pendingAutoRef = useRef<MediaFile[]>([])
  const uploadingRef = useRef(false)
  const filesRef = useRef<MediaFile[]>([])
  const [statuses, setStatuses] = useState<Record<string, FileStatus>>({})
  const [skipList, setSkipList] = useState<SkipList>({})
  const [activeKey, setActiveKey] = useState<string | null>(null)
  const [activePath, setActivePath] = useState<string | null>(null)
  const [result, setResult] = useState<UploadDoneEvent | null>(null)
  const [commandError, setCommandError] = useState<string | null>(null)

  useEffect(() => {
    invoke<R2Config | null>("load_config")
      .then((saved) => {
        if (saved) setConfig({ ...DEFAULT_CONFIG, ...saved })
      })
      .catch(() => { })
      .finally(() => {
        configLoaded.current = true
      })
    invoke<SkipList>("get_skip_list")
      .then((s) => setSkipList(s ?? {}))
      .catch(() => { })
  }, [])

  // Persist the delete-after-upload preference.
  useEffect(() => {
    localStorage.setItem(DELETE_KEY, deleteAfterUpload ? "1" : "0")
  }, [deleteAfterUpload])

  // Upload progress events: subscribed once; every handler only does
  // functional state updates, so no per-batch teardown is needed.
  useEffect(() => {
    const subs: Promise<UnlistenFn>[] = [
      listen<PrepareStartEvent>("upload://prepare-start", (e) => {
        setStatuses((prev) => {
          const cur = prev[e.payload.path]
          if (!cur) return prev
          return {
            ...prev,
            [e.payload.path]: { ...cur, state: "preparing", action: e.payload.action, preparePct: 0 },
          }
        })
      }),
      listen<PrepareProgressEvent>("upload://prepare-progress", (e) => {
        setStatuses((prev) => {
          const cur = prev[e.payload.path]
          if (!cur || cur.state !== "preparing") return prev
          const pct =
            e.payload.duration > 0
              ? Math.min(100, Math.round((e.payload.seconds / e.payload.duration) * 100))
              : 0
          return { ...prev, [e.payload.path]: { ...cur, preparePct: pct } }
        })
      }),
      listen<FileStartEvent>("upload://file-start", (e) => {
        setStatuses((prev) => ({
          ...prev,
          [e.payload.path]: { state: "active", uploaded: 0 },
        }))
        setActiveKey(e.payload.key)
        setActivePath(e.payload.path)
      }),
      listen<FileProgressEvent>("upload://progress", (e) => {
        setStatuses((prev) => {
          const cur = prev[e.payload.path]
          if (!cur) return prev
          return { ...prev, [e.payload.path]: { ...cur, uploaded: e.payload.uploaded } }
        })
      }),
      listen<FileDoneEvent>("upload://file-done", (e) => {
        setStatuses((prev) => {
          const cur = prev[e.payload.path]
          if (!cur) return prev
          return {
            ...prev,
            [e.payload.path]: { state: "done", uploaded: cur.uploaded, warning: e.payload.warning },
          }
        })
        // Once uploaded, the file no longer needs preparation next time.
        setTranscodeSet((prev) => updateSet(prev, (next) => next.delete(e.payload.path)))
      }),
      listen<FileErrorEvent>("upload://file-error", (e) => {
        setStatuses((prev) => ({
          ...prev,
          [e.payload.path]: {
            state: "error",
            uploaded: prev[e.payload.path]?.uploaded ?? 0,
            error: e.payload.error === "cancelled" ? "Cancelled" : e.payload.error,
          },
        }))
      }),
    ]
    return () => {
      Promise.all(subs).then((fns) => fns.forEach((fn) => fn()))
    }
  }, [])

  // Keep refs in sync for async callbacks (watcher, pump) that must read
  // fresh values without re-subscribing. One effect covers them all.
  useEffect(() => {
    autoUploadRef.current = autoUpload
    autoActiveRef.current = autoActive
    uploadingRef.current = uploading
    filesRef.current = files
  })

  // Persist the non-secret settings whenever they change.
  useEffect(() => {
    if (!configLoaded.current) return
    invoke("save_config", { config }).catch(() => { })
  }, [config])

  // Persist the secret to the OS keychain, but only when the user actually
  // types one — leaving the field blank means "use the stored one". Writes are
  // debounced so the keychain prompt appears at most once per edit.
  useEffect(() => {
    if (!configLoaded.current) return
    const secret = config.secret_access_key.trim()
    if (secret.length === 0) return // never overwrite stored with empty
    const timer = setTimeout(() => {
      invoke("save_secret", { secret: config.secret_access_key }).catch(() => { })
    }, 700)
    return () => clearTimeout(timer)
  }, [config.secret_access_key])

  // Watch the selected folder for changes and merge new files into the list.
  useEffect(() => {
    if (!folder) return

    invoke("start_watching", { path: folder }).catch(() => {})

    const unlisten = listen<string>("folder-changed", async () => {
      try {
        const media = await invoke<MediaFile[]>("scan_folder", { path: folder })
        // Compute changes against the current file list synchronously —
        // state updaters run later, so they can't feed decisions below.
        const prevFiles = filesRef.current
        const onDisk = new Set(media.map((f) => f.path))
        const kept = prevFiles.filter((f) => onDisk.has(f.path))
        const existingPaths = new Set(kept.map((f) => f.path))
        const added = media.filter((f) => !existingPaths.has(f.path))
        const next = [...kept, ...added].sort((a, b) => a.relative.localeCompare(b.relative))
        if (next.length === 0) setScanError("No media files found in this folder.")
        else if (prevFiles.length === 0) setScanError(null)
        const changed = !(next.length === prevFiles.length && next.every((f, i) => f.path === prevFiles[i].path))
        if (changed) {
          filesRef.current = next
          setFiles(next)
          setResult(null)
        }
        // Files vanished while uploading: the backend may be wedged on I/O
        // for one of them. Skip just that file; the batch continues.
        if (kept.length < prevFiles.length && uploadingRef.current) {
          invoke("skip_current_file").catch(() => {})
        }
        if (autoUploadRef.current && autoActiveRef.current && added.length > 0) {
          // New arrivals are always fresh — clear any stale exclusion, then
          // queue them; the pump effect uploads them when idle.
          setExcluded((prev) => {
            const n = new Set(prev)
            for (const f of added) n.delete(f.path)
            return n
          })
          pendingAutoRef.current.push(...added)
        }
      } catch {
        // ignore scan errors from watcher
      }
    })

    return () => {
      unlisten.then((fn) => fn())
      invoke("stop_watching").catch(() => {})
    }
  }, [folder])

  const counts = useMemo(() => {
    const c = { all: 0, image: 0, video: 0, audio: 0 } as Record<"all" | MediaKind, number>
    for (const f of files) c[f.kind] += 1
    c.all = files.length
    return c
  }, [files])

  const selected = useMemo(
    () => files.filter((f) => !excluded.has(f.path)),
    [files, excluded],
  )
  const selectedBytes = useMemo(
    () => selected.reduce((sum, f) => sum + f.size, 0),
    [selected],
  )

  const visible = useMemo(
    () => (filter === "all" ? files : files.filter((f) => f.kind === filter)),
    [files, filter],
  )

  const uploadedBytes = useMemo(() => {
    let sum = 0
    for (const f of selected) {
      const st = statuses[f.path]
      if (!st) continue
      if (st.state === "done") sum += f.size
      else sum += Math.min(st.uploaded, f.size)
    }
    return sum
  }, [selected, statuses])

  const configValid =
    config.bucket.trim().length > 0 &&
    config.access_key_id.trim().length > 0 &&
    (config.account_id.trim().length > 0 || config.endpoint.trim().length > 0)

  const hasCredentials = Object.values(config).some((v) => v.trim().length > 0)

  // Armed auto-upload pump: whenever idle and armed, drain the watcher's
  // queue; if empty, upload whatever is currently selected (covers files the
  // user checked after arming, and leftovers found by the post-batch scan).
  useEffect(() => {
    if (!autoActive || uploading) return
    const batch = pendingAutoRef.current.splice(0)
    const toUpload = batch.length > 0 ? batch : selected
    if (toUpload.length > 0) {
      startUpload(toUpload)
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [uploading, autoActive, selected])

  async function pickFolder() {
    setScanError(null)
    setResult(null)
    setStatuses({})
    setActiveKey(null)
    try {
      const path = await invoke<string | null>("select_folder")
      if (!path) return
      setFolder(path)
      setScanning(true)
      setFilter("all")
      const [media, hist, skip] = await Promise.all([
        invoke<MediaFile[]>("scan_folder", { path }),
        invoke<History>("get_history").catch<History>(() => ({})),
        invoke<SkipList>("get_skip_list").catch<SkipList>(() => ({})),
      ])
      setSkipList(skip ?? {})
      const initialExcluded = new Set<string>()
      for (const f of media) {
        const h = hist[f.path]
        const alreadyUploaded = h !== undefined && h.size === f.size && h.mtime === f.mtime
        if (alreadyUploaded || skip[f.path] !== undefined) initialExcluded.add(f.path)
      }
      setExcluded(initialExcluded)
      filesRef.current = media
      setFiles(media)
      if (media.length === 0) {
        setScanError("No media files found in this folder.")
      }
    } catch (e) {
      setFolder(null)
      filesRef.current = []
      setFiles([])
      setScanError(String(e))
    } finally {
      setScanning(false)
    }
  }

  function toggleFile(path: string) {
    const wasSkipped = skipList[path] !== undefined
    const becomingIncluded = excluded.has(path)
    setExcluded((prev) =>
      updateSet(prev, (next) => (next.has(path) ? next.delete(path) : next.add(path))),
    )
    if (wasSkipped && becomingIncluded) {
      const next = { ...skipList }
      delete next[path]
      setSkipList(next)
      invoke("remove_skip", { path }).catch(() => { })
    }
  }

  function skipFile(path: string) {
    setExcluded((prev) => updateSet(prev, (next) => next.add(path)))
    setSkipList((prev) => ({ ...prev, [path]: Math.floor(Date.now() / 1000) }))
    invoke("add_skip", { path }).catch(() => { })
  }

  function unskipFile(path: string) {
    setSkipList((prev) => {
      const next = { ...prev }
      delete next[path]
      return next
    })
    invoke("remove_skip", { path }).catch(() => { })
  }

  function toggleVisible(check: boolean) {
    setExcluded((prev) =>
      updateSet(prev, (next) => {
        for (const f of visible) {
          if (check) next.delete(f.path)
          else next.add(f.path)
        }
      }),
    )
  }

  async function cancelUpload() {
    if (cancelling) return
    setAutoActive(false)
    pendingAutoRef.current = []
    setCancelling(true)
    try {
      await invoke("cancel_upload")
    } catch {
      // ignore
    }
  }

  /** Secret from the form, else the cached one, else the OS keychain. */
  async function resolveSecret(): Promise<string | null> {
    const typed = config.secret_access_key.trim()
    if (typed.length > 0) return typed
    if (secretRef.current) return secretRef.current
    const stored = await invoke<string | null>("load_secret").catch<null>(() => null)
    const secret = (stored ?? "").trim()
    if (secret.length > 0) secretRef.current = secret
    return secret
  }

  async function startUpload(autoFiles?: MediaFile[]) {
    if (uploadingRef.current || !folder) return
    const source = autoFiles ?? selected

    const secret = await resolveSecret()
    if (!secret) {
      setCommandError("Enter your R2 Secret Access Key to upload.")
      return
    }
    // Clicking Auto Upload arms the watcher even with nothing selected yet.
    if (autoUpload) setAutoActive(true)
    if (source.length === 0) return

    const items: UploadItem[] = source.map((f) => ({
      path: f.path,
      relative: f.relative,
      size: f.size,
      transcode: f.kind === "video" && transcodeSet.has(f.path),
    }))
    const initial: Record<string, FileStatus> = {}
    for (const f of source) initial[f.path] = { state: "pending", uploaded: 0 }
    setStatuses(initial)
    setResult(null)
    setCommandError(null)
    setActiveKey(null)
    setCancelling(false)
    setUploading(true)
    uploadingRef.current = true

    try {
      const done = await invoke<UploadDoneEvent>("upload_files", {
        config: { ...config, secret_access_key: secret },
        items,
        root: folder,
        deleteAfterUpload,
      })
      setResult(done)
    } catch (e) {
      setCommandError(String(e))
    } finally {
      setActiveKey(null)
      setActivePath(null)
      setCancelling(false)
      await rescan(items)
      uploadingRef.current = false
      setUploading(false)
    }
  }

  /** Sync the file list with disk after a batch and exclude what was processed. */
  async function rescan(processed: UploadItem[]) {
    if (!folder) return
    try {
      const media = await invoke<MediaFile[]>("scan_folder", { path: folder })
      filesRef.current = media
      setFiles(media)
      const onDisk = new Set(media.map((f) => f.path))
      setStatuses((prev) => {
        const next: Record<string, FileStatus> = {}
        for (const [k, v] of Object.entries(prev)) {
          if (onDisk.has(k) && v.state !== "error") next[k] = v
        }
        return next
      })
      setExcluded((prev) => updateSet(prev, (next) => {
        for (const item of processed) next.add(item.path)
      }))
    } catch {
      // ignore scan errors
    }
  }

  const overallPct =
    selectedBytes > 0 ? Math.round((uploadedBytes / selectedBytes) * 100) : 0

  return (
    <main className="app">
      <header className="header">
        <div className="header-title">
          <img src={logo} className="logo" alt="Cloud Storage Bridge" />
          <div>
            <h1>
              Cloud Storage Bridge
              {import.meta.env.DEV && <span className="dev-badge">dev</span>}
            </h1>
            <p>Upload a folder's media to a Cloudflare R2 bucket</p>
          </div>
        </div>
      </header>

      <section className="card">
        <div className="card-header">
          <h2 className="card-title">
            <button
              className="collapse-toggle"
              onClick={() => setDestOpen((open) => !open)}
              aria-expanded={destOpen}
            >
              <span className={`chevron ${destOpen ? "chevron-open" : ""}`}>▸</span>
              Destination
            </button>
          </h2>
          {hasCredentials && (
            <button
              className="btn ghost btn-clear-creds"
              onClick={() => {
                setConfig({ ...DEFAULT_CONFIG })
                invoke("save_secret", { secret: "" }).catch(() => { })
              }}
              disabled={uploading}
              title="Clear all fields and remove the saved secret from the OS keychain"
            >
              Clear credentials
            </button>
          )}
        </div>
        {destOpen && (
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
        )}
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

            <div className="file-list">
              <table className="file-table">
                <colgroup>
                  <col className="col-check" />
                  <col className="col-name" />
                  <col className="col-transcript" />
                  <col className="col-skip" />
                  <col className="col-status" />
                  <col className="col-size" />
                  <col className="col-remove" />
                </colgroup>
                <thead>
                  <tr className="file-header">
                    <th />
                    <th className="th-name">Name</th>
                    <th className="th-transcript">Transcript</th>
                    <th className="th-skip">Skip</th>
                    <th className="th-status">Status</th>
                    <th className="th-size">Size</th>
                    <th className="th-remove">Remove</th>
                  </tr>
                </thead>
                <tbody>
                  {visible.map((f) => {
                    const st = statuses[f.path]
                    const included = !excluded.has(f.path)
                    const skipped = skipList[f.path] !== undefined
                    const sv = statusView(st, f.size, uploading)
                    return (
                      <tr
                        key={f.path}
                        className={`file-row ${st?.state === "error" ? "file-error" : ""} ${skipped ? "file-skipped" : ""}`}
                      >
                        <td className="td-check">
                          <input
                            type="checkbox"
                            checked={included}
                            onChange={() => toggleFile(f.path)}
                            disabled={f.path === activePath}
                          />
                        </td>
                        <td className="td-name">
                          <span className="file-name" title={f.path}>
                            {f.relative}
                          </span>
                        </td>
                        <td className="td-transcript">
                          {f.kind === "video" ? (
                            <input
                              type="checkbox"
                              className="switch"
                              role="switch"
                              checked={transcodeSet.has(f.path)}
                              onChange={() =>
                                setTranscodeSet((prev) => {
                                  const next = new Set(prev)
                                  if (next.has(f.path)) next.delete(f.path)
                                  else next.add(f.path)
                                  return next
                                })
                              }
                              disabled={f.path === activePath}
                              title="Prepare for browser playback (H.264 MP4 + WebVTT subtitles)"
                            />
                          ) : null}
                        </td>
                        <td className="td-skip">
                          <input
                            type="checkbox"
                            className="switch"
                            role="switch"
                            checked={skipped}
                            onChange={() => (skipped ? unskipFile(f.path) : skipFile(f.path))}
                            disabled={f.path === activePath}
                            title={skipped ? "Un-skip this file" : "Don't upload this file; remember the choice"}
                          />
                        </td>
                        <td className="td-status">
                          <div className="row-state">
                            {st?.warning && (
                              <span className="tag tag-warn" title={st.warning}>
                                May not play in browser
                              </span>
                            )}
                            {sv && <span className={`status ${sv.cls}`}>{sv.text}</span>}
                          </div>
                        </td>
                        <td className="td-size">
                          <span className="file-size">{formatBytes(f.size)}</span>
                        </td>
                        <td className="td-remove">
                          <button
                            className="btn btn-ghost btn-del"
                            disabled={f.path === activePath}
                            title="Delete file from disk"
                            onClick={async () => {
                              const ok = window.confirm(`Delete ${f.relative}?\nThis cannot be undone.`)
                              if (!ok) return
                              await invoke("delete_file", { path: f.path })
                              filesRef.current = filesRef.current.filter((x) => x.path !== f.path)
                              setFiles(filesRef.current)
                              if (filesRef.current.length === 0) {
                                setScanError("No media files found in this folder.")
                              }
                              setExcluded((prev) => updateSet(prev, (next) => next.delete(f.path)))
                              setStatuses((prev) => {
                                const next = { ...prev }
                                delete next[f.path]
                                return next
                              })
                              setTranscodeSet((prev) => updateSet(prev, (next) => next.delete(f.path)))
                            }}
                          >
                            🗑
                          </button>
                        </td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
            </div>
          </>
        )}
        {scanError && <p className="hint">{scanError}</p>}
      </section>

      {folder && (
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
            {uploading || autoActive ? (
              <button className="btn danger" onClick={cancelUpload} disabled={cancelling}>
                {cancelling ? "Cancelling..." : "Cancel"}
              </button>
            ) : (
              <button
                className="btn primary"
                onClick={() => startUpload()}
                disabled={!configValid || (!autoUpload && selected.length === 0)}
              >
                {autoUpload ? "Auto Upload" : "Upload to R2"}
              </button>
            )}
            <Toggle
              label="Delete after upload"
              title="Remove files locally after they finish uploading"
              checked={deleteAfterUpload}
              disabled={uploading || autoActive}
              onChange={setDeleteAfterUpload}
            />
            <Toggle
              label="Auto upload"
              title="Automatically upload every new file added to this folder"
              checked={autoUpload}
              disabled={uploading || autoActive}
              onChange={setAutoUpload}
            />
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
  )
}

export default App
