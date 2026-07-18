"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

type ItemType = "text" | "html" | "code" | "url" | "image";
type Section = "vault" | "dashboard" | "activity" | "trash" | "settings";
type PollState = "idle" | "polling" | "paused" | "error";

type VaultItem = {
  id: string;
  payload: string;
  type: ItemType;
  sourceUrl: string | null;
  contentHash: string;
  sizeBytes: number;
  createdAt: string;
  deletedAt: string | null;
  tags: string[];
  pinned: boolean;
};

type Stats = {
  total_items: number;
  total_size_bytes: number;
  pinned_items: number;
  type_breakdown: Record<string, number>;
  daily_counts: { date: string; count: number }[];
  avg_payload_size: number;
};

type Activity = {
  id: number;
  action: string;
  itemId: string | null;
  detail: string | null;
  ip: string | null;
  createdAt: string;
};

type Toast = {
  id: number;
  type: "success" | "error" | "warning" | "info";
  message: string;
};

const TYPE_META: Record<ItemType, { label: string; glyph: string }> = {
  text: { label: "Text", glyph: "T" },
  html: { label: "HTML", glyph: "<>" },
  code: { label: "Code", glyph: "{}" },
  url: { label: "Link", glyph: "↗" },
  image: { label: "Image", glyph: "▧" },
};

const NAV: { id: Section; label: string; glyph: string }[] = [
  { id: "vault", label: "Vault", glyph: "⌁" },
  { id: "dashboard", label: "Overview", glyph: "⌗" },
  { id: "activity", label: "Activity", glyph: "↺" },
  { id: "trash", label: "Trash", glyph: "⌫" },
  { id: "settings", label: "Settings", glyph: "⚙" },
];

function detectSubType(text: string): ItemType {
  if (text.startsWith("http://") || text.startsWith("https://")) return "url";
  if (!text || text.length < 20) return "text";
  const indicators = [
    /^(function|const|let|var|import|export|class|def |#include)/m.test(text),
    /[{}=;()]/.test(text) && text.includes("\n"),
    text.split("\n").length > 3 && /\s{2,}|\t/.test(text),
  ];
  return indicators.filter(Boolean).length >= 2 ? "code" : "text";
}

async function hashValue(value: string) {
  const bytes = new TextEncoder().encode(value);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
}

function timeAgo(value: string) {
  const delta = Date.now() - new Date(value).getTime();
  const mins = Math.max(0, Math.floor(delta / 60_000));
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return days < 7
    ? `${days}d ago`
    : new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" }).format(
        new Date(value),
      );
}

function previewText(item: VaultItem) {
  if (item.type === "image") return "Clipboard image";
  return item.payload.replace(/\s+/g, " ").trim();
}

function apiMessage(payload: unknown, fallback: string) {
  if (
    payload &&
    typeof payload === "object" &&
    "error" in payload &&
    payload.error &&
    typeof payload.error === "object" &&
    "message" in payload.error
  ) {
    return String(payload.error.message);
  }
  return fallback;
}

async function imageBlobToPayload(blob: Blob) {
  let working = blob;
  if (blob.size > 500 * 1024) {
    const bitmap = await createImageBitmap(blob);
    const ratio = Math.min(1, 1200 / Math.max(bitmap.width, bitmap.height));
    const canvas = document.createElement("canvas");
    canvas.width = Math.round(bitmap.width * ratio);
    canvas.height = Math.round(bitmap.height * ratio);
    canvas.getContext("2d")?.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
    working =
      (await new Promise<Blob | null>((resolve) =>
        canvas.toBlob(resolve, "image/webp", 0.8),
      )) ?? blob;
  }
  return await new Promise<string>((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(working);
  });
}

function openQueue() {
  return new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open("clipboardQueue", 1);
    request.onupgradeneeded = () => {
      request.result.createObjectStore("pending", { keyPath: "content_hash" });
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function queueOffline(payload: Record<string, unknown>) {
  const db = await openQueue();
  const transaction = db.transaction("pending", "readwrite");
  transaction.objectStore("pending").put(payload);
  await new Promise<void>((resolve, reject) => {
    transaction.oncomplete = () => resolve();
    transaction.onerror = () => reject(transaction.error);
  });
  db.close();
}

export function VaultApp({
  initialSection = "vault",
  displayName,
}: {
  initialSection?: Section;
  displayName?: string;
}) {
  const [section, setSection] = useState<Section>(initialSection);
  const [items, setItems] = useState<VaultItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [query, setQuery] = useState("");
  const [typeFilter, setTypeFilter] = useState("all");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [expanded, setExpanded] = useState<VaultItem | null>(null);
  const [pollState, setPollState] = useState<PollState>("idle");
  const [lastPoll, setLastPoll] = useState<Date | null>(null);
  const [stats, setStats] = useState<Stats | null>(null);
  const [activity, setActivity] = useState<Activity[]>([]);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);
  const [columnsOpen, setColumnsOpen] = useState(false);
  const [columns, setColumns] = useState({ type: true, size: true, date: true });
  const [retention, setRetention] = useState({
    max_items: 10000,
    max_age_days: 365,
    max_total_size_mb: 500,
    action_on_limit: "delete_oldest",
  });
  const hashes = useRef(new Set<string>());
  const toastId = useRef(0);
  const searchRef = useRef<HTMLInputElement>(null);

  const addToast = useCallback((type: Toast["type"], message: string) => {
    const id = ++toastId.current;
    setToasts((current) => [...current.slice(-3), { id, type, message }]);
    if (type !== "error") {
      const duration = type === "success" ? 3000 : type === "info" ? 5000 : 8000;
      window.setTimeout(
        () => setToasts((current) => current.filter((toast) => toast.id !== id)),
        duration,
      );
    }
  }, []);

  const loadItems = useCallback(async () => {
    setLoading(true);
    try {
      const params = new URLSearchParams({ limit: "100" });
      if (query.trim()) params.set("q", query.trim());
      if (typeFilter !== "all") params.set("type", typeFilter);
      if (section === "trash") params.set("trash", "true");
      const response = await fetch(`/items?${params}`, { cache: "no-store" });
      const data = (await response.json()) as { items?: VaultItem[] };
      if (!response.ok) throw new Error(apiMessage(data, "Could not load the vault"));
      const nextItems = data.items ?? [];
      setItems(nextItems);
      hashes.current = new Set(nextItems.map((item) => item.contentHash));
      setSelected(new Set());
    } catch (error) {
      addToast("error", error instanceof Error ? error.message : "Could not load the vault");
    } finally {
      setLoading(false);
    }
  }, [addToast, query, section, typeFilter]);

  const loadStats = useCallback(async () => {
    const response = await fetch("/admin/stats", { cache: "no-store" });
    if (response.ok) setStats((await response.json()) as Stats);
  }, []);

  const loadActivity = useCallback(async () => {
    const response = await fetch("/admin/activity", { cache: "no-store" });
    if (response.ok) {
      const data = (await response.json()) as { activity: Activity[] };
      setActivity(data.activity);
    }
  }, []);

  useEffect(() => {
    const saved = localStorage.getItem("vault-columns");
    if (saved) {
      try {
        setColumns(JSON.parse(saved));
      } catch {
        localStorage.removeItem("vault-columns");
      }
    }
    if ("serviceWorker" in navigator) {
      void navigator.serviceWorker.register("/sw.js").catch(() => undefined);
    }
  }, []);

  useEffect(() => {
    const timer = window.setTimeout(() => {
      if (section === "vault" || section === "trash") void loadItems();
      if (section === "dashboard") void loadStats();
      if (section === "activity") void loadActivity();
      if (section === "settings") {
        void fetch("/settings")
          .then((response) => response.json())
          .then((data) => data.settings && setRetention(data.settings));
      }
    }, 180);
    return () => window.clearTimeout(timer);
  }, [loadActivity, loadItems, loadStats, section]);

  const submitClip = useCallback(
    async (clip: { payload: string; type: ItemType; source_url?: string }) => {
      const contentHash = await hashValue(clip.payload);
      if (hashes.current.has(contentHash)) return false;
      const body = { ...clip, content_hash: contentHash };
      if (!navigator.onLine) {
        await queueOffline(body);
        addToast("warning", "Saved offline. It will sync when you reconnect.");
        return false;
      }
      const response = await fetch("/items", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      const data = await response.json();
      if (response.status === 409) {
        hashes.current.add(contentHash);
        return false;
      }
      if (!response.ok) throw new Error(apiMessage(data, "Could not save clipboard item"));
      const item = data.item as VaultItem;
      hashes.current.add(item.contentHash);
      setItems((current) => [item, ...current]);
      addToast("success", `${TYPE_META[item.type].label} clip saved`);
      return true;
    },
    [addToast],
  );

  const readClipboard = useCallback(
    async (quiet = false) => {
      if (!navigator.clipboard || !window.isSecureContext) {
        setPollState("error");
        if (!quiet) addToast("error", "Clipboard access needs a secure browser context");
        return;
      }
      if (document.hidden || !document.hasFocus()) {
        setPollState("paused");
        return;
      }
      setPollState("polling");
      try {
        let added = false;
        if (navigator.clipboard.read) {
          const clipboardItems = await navigator.clipboard.read();
          for (const item of clipboardItems) {
            const imageType = item.types.find((type) => type.startsWith("image/"));
            if (imageType) {
              const blob = await item.getType(imageType);
              added = (await submitClip({ payload: await imageBlobToPayload(blob), type: "image" })) || added;
            } else if (item.types.includes("text/plain")) {
              const text = await (await item.getType("text/plain")).text();
              added = (await submitClip({ payload: text, type: detectSubType(text) })) || added;
            } else if (item.types.includes("text/html")) {
              const html = await (await item.getType("text/html")).text();
              added = (await submitClip({ payload: html, type: "html" })) || added;
            }
          }
        } else {
          const text = await navigator.clipboard.readText();
          if (text) added = await submitClip({ payload: text, type: detectSubType(text) });
        }
        setLastPoll(new Date());
        if (!quiet && !added) addToast("info", "Clipboard checked — nothing new");
      } catch (error) {
        const denied = error instanceof DOMException && error.name === "NotAllowedError";
        setPollState(denied ? "error" : "idle");
        if (!quiet) {
          addToast(
            denied ? "warning" : "error",
            denied
              ? "Allow clipboard access, then try again"
              : "Clipboard could not be read",
          );
        }
      }
    },
    [addToast, submitClip],
  );

  useEffect(() => {
    const pause = () => setPollState("paused");
    const resume = () => {
      setPollState("polling");
      void readClipboard(true);
    };
    const visibility = () => (document.hidden ? pause() : resume());
    window.addEventListener("blur", pause);
    window.addEventListener("focus", resume);
    document.addEventListener("visibilitychange", visibility);
    const timer = window.setInterval(() => void readClipboard(true), 2000);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener("blur", pause);
      window.removeEventListener("focus", resume);
      document.removeEventListener("visibilitychange", visibility);
    };
  }, [readClipboard]);

  useEffect(() => {
    const replay = async () => {
      try {
        const db = await openQueue();
        const tx = db.transaction("pending", "readwrite");
        const store = tx.objectStore("pending");
        const queued = await new Promise<Record<string, unknown>[]>((resolve, reject) => {
          const request = store.getAll();
          request.onsuccess = () => resolve(request.result);
          request.onerror = () => reject(request.error);
        });
        let replayed = 0;
        for (const clip of queued) {
          const response = await fetch("/items", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: JSON.stringify(clip),
          });
          if (response.ok || response.status === 409) {
            store.delete(String(clip.content_hash));
            replayed += 1;
          }
        }
        db.close();
        if (replayed) {
          addToast("success", `${replayed} offline clip${replayed === 1 ? "" : "s"} synced`);
          void loadItems();
        }
      } catch {
        // The next online event will retry.
      }
    };
    window.addEventListener("online", replay);
    void replay();
    return () => window.removeEventListener("online", replay);
  }, [addToast, loadItems]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement;
      const editing = target.tagName === "INPUT" || target.tagName === "TEXTAREA";
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "f") {
        event.preventDefault();
        searchRef.current?.focus();
      } else if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a" && !editing) {
        event.preventDefault();
        setSelected(new Set(items.map((item) => item.id)));
      } else if (event.key === "Delete" && selected.size && !editing) {
        void bulkDelete();
      } else if (event.key === "Escape") {
        setExpanded(null);
        setSelected(new Set());
        setShortcutsOpen(false);
      } else if (event.key === "?" && !editing) {
        setShortcutsOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const mutateItem = async (id: string, body: Record<string, unknown>) => {
    const response = await fetch(`/items/${id}`, {
      method: "PATCH",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const data = await response.json();
    if (!response.ok) throw new Error(apiMessage(data, "Could not update item"));
    setItems((current) =>
      current.map((item) => (item.id === id ? (data.item as VaultItem) : item)),
    );
  };

  const deleteItem = async (item: VaultItem, permanent = false) => {
    const response = await fetch(`/items/${item.id}${permanent ? "?permanent=true" : ""}`, {
      method: "DELETE",
    });
    if (!response.ok && response.status !== 204) {
      const data = await response.json();
      throw new Error(apiMessage(data, "Could not delete item"));
    }
    setItems((current) => current.filter((entry) => entry.id !== item.id));
    addToast("success", permanent ? "Item permanently deleted" : "Moved to trash");
  };

  const restoreItem = async (item: VaultItem) => {
    const response = await fetch(`/items/${item.id}/restore`, { method: "POST" });
    if (!response.ok) throw new Error("Could not restore item");
    setItems((current) => current.filter((entry) => entry.id !== item.id));
    addToast("success", "Item restored to the vault");
  };

  const bulkDelete = async () => {
    if (!selected.size) return;
    const params = new URLSearchParams({ ids: Array.from(selected).join(",") });
    const response = await fetch(`/items?${params}`, { method: "DELETE" });
    if (!response.ok) {
      addToast("error", "Could not delete selected items");
      return;
    }
    setItems((current) => current.filter((item) => !selected.has(item.id)));
    addToast("success", `${selected.size} item${selected.size === 1 ? "" : "s"} moved to trash`);
    setSelected(new Set());
  };

  const copyItem = async (item: VaultItem) => {
    try {
      if (item.type === "image") {
        const blob = await (await fetch(item.payload)).blob();
        await navigator.clipboard.write([new ClipboardItem({ [blob.type]: blob })]);
      } else {
        await navigator.clipboard.writeText(item.payload);
      }
      addToast("success", "Copied back to clipboard");
    } catch {
      addToast("error", "Could not copy this item");
    }
  };

  const toggleSelected = (id: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const maxType = Math.max(1, ...Object.values(stats?.type_breakdown ?? {}));
  const greeting = displayName?.split(" ")[0] ?? "there";

  return (
    <div className="vault-shell">
      <aside className={`sidebar ${sidebarOpen ? "open" : ""}`}>
        <div className="brand-row">
          <span className="brand-mark" aria-hidden="true"><i /><b /><i /></span>
          <div><strong>Clipboard</strong><span>Vault</span></div>
        </div>
        <nav aria-label="Primary navigation">
          {NAV.map((entry) => (
            <button
              key={entry.id}
              className={section === entry.id ? "active" : ""}
              onClick={() => { setSection(entry.id); setSidebarOpen(false); }}
            >
              <span>{entry.glyph}</span>{entry.label}
              {entry.id === "trash" && <small>30d</small>}
            </button>
          ))}
        </nav>
        <div className="sidebar-note">
          <span className={`poll-dot ${pollState}`} />
          <div><strong>{pollState === "polling" ? "Watching clipboard" : pollState === "paused" ? "Paused while away" : pollState === "error" ? "Permission needed" : "Ready to collect"}</strong><small>Browser-local capture · encrypted transit</small></div>
        </div>
        <button className="sidebar-help" onClick={() => setShortcutsOpen(true)}><span>?</span> Keyboard shortcuts</button>
      </aside>

      <main>
        <header className="topbar">
          <button className="mobile-menu" onClick={() => setSidebarOpen((open) => !open)} aria-label="Toggle menu">☰</button>
          <div className="global-search">
            <span>⌕</span>
            <input ref={searchRef} value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search everything you copied…" aria-label="Search clipboard" />
            <kbd>⌘ F</kbd>
          </div>
          <button className="read-now" onClick={() => void readClipboard()}><span>+</span> Read clipboard</button>
          <button className="avatar" aria-label="Account menu">{greeting.slice(0, 1).toUpperCase()}</button>
        </header>

        <div className="content-wrap">
          {section === "vault" || section === "trash" ? (
            <>
              <div className="page-heading">
                <div>
                  <p className="eyebrow">{section === "trash" ? "RECOVERY WINDOW" : "PRIVATE CLIPBOARD MEMORY"}</p>
                  <h1>{section === "trash" ? "Recently deleted" : `Good ${new Date().getHours() < 12 ? "morning" : new Date().getHours() < 18 ? "afternoon" : "evening"}, ${greeting}.`}</h1>
                  <p>{section === "trash" ? "Restore anything you still need. Items leave the vault after 30 days." : "Everything you copy, ready when you need it."}</p>
                </div>
                <div className="poll-pill" title="Clipboard polling status">
                  <span className={`poll-dot ${pollState}`} />
                  <div><strong>{pollState === "polling" ? "Live capture" : pollState === "paused" ? "Capture paused" : pollState === "error" ? "Access needed" : "Capture ready"}</strong><small>{lastPoll ? `Last check ${lastPoll.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}` : "Click Read clipboard to begin"}</small></div>
                </div>
              </div>

              <div className="vault-toolbar">
                <div className="filter-tabs" role="tablist" aria-label="Filter by clip type">
                  {["all", "text", "code", "url", "image"].map((type) => (
                    <button key={type} className={typeFilter === type ? "active" : ""} onClick={() => setTypeFilter(type)}>{type === "all" ? "All clips" : TYPE_META[type as ItemType].label}</button>
                  ))}
                </div>
                <div className="toolbar-actions">
                  <button className="select-visible" onClick={() => setSelected(selected.size === items.length ? new Set() : new Set(items.map((item) => item.id)))}>{selected.size === items.length && items.length ? "Clear page" : "Select page"}</button>
                  <a className="icon-button" href={`/items/export?format=json${selected.size ? `&ids=${Array.from(selected).join(",")}` : ""}`} title="Export JSON">⇩</a>
                  <button className="icon-button" onClick={() => setColumnsOpen((open) => !open)} title="Choose columns">☷</button>
                  {columnsOpen && (
                    <div className="columns-popover">
                      <strong>Card details</strong>
                      {Object.entries(columns).map(([key, value]) => (
                        <label key={key}><input type="checkbox" checked={value} onChange={(event) => { const next = { ...columns, [key]: event.target.checked }; setColumns(next); localStorage.setItem("vault-columns", JSON.stringify(next)); }} />{key}</label>
                      ))}
                    </div>
                  )}
                </div>
              </div>

              {selected.size > 0 && (
                <div className="selection-bar">
                  <strong>{selected.size} selected</strong>
                  <button onClick={() => void bulkDelete()}>Move to trash</button>
                  <a href={`/items/export?format=json&ids=${Array.from(selected).join(",")}`}>Export</a>
                  <button onClick={() => setSelected(new Set())}>Clear</button>
                </div>
              )}

              <section className="clip-list" role="region" aria-live="polite" aria-label="Clipboard items">
                {loading ? Array.from({ length: 4 }, (_, index) => <SkeletonCard key={index} />) : items.length === 0 ? (
                  <EmptyState trash={section === "trash"} onRead={() => void readClipboard()} />
                ) : items.map((item) => (
                  <article key={item.id} className={`clip-card ${selected.has(item.id) ? "selected" : ""}`}>
                    <label className="check-wrap"><input type="checkbox" checked={selected.has(item.id)} onChange={() => toggleSelected(item.id)} aria-label={`Select ${TYPE_META[item.type].label} clip`} /><span /></label>
                    <button className={`type-icon ${item.type}`} onClick={() => setExpanded(item)} aria-label={`Open ${TYPE_META[item.type].label} clip`}>{TYPE_META[item.type].glyph}</button>
                    <button className="clip-main" onClick={() => setExpanded(item)}>
                      <span className="clip-preview" dir="auto">{previewText(item)}</span>
                      <span className="clip-meta">
                        {columns.type && <b className={`type-label ${item.type}`}>{TYPE_META[item.type].label}</b>}
                        {columns.size && <i>{formatBytes(item.sizeBytes)}</i>}
                        {item.tags.map((tag) => <em key={tag}>#{tag}</em>)}
                        {columns.date && <i>{timeAgo(item.createdAt)}</i>}
                      </span>
                    </button>
                    <div className="clip-actions">
                      {section === "trash" ? <>
                        <button onClick={() => void restoreItem(item)} title="Restore">↺</button>
                        <button className="danger" onClick={() => void deleteItem(item, true)} title="Delete permanently">×</button>
                      </> : <>
                        <button className={item.pinned ? "pinned" : ""} onClick={() => void mutateItem(item.id, { pinned: !item.pinned }).catch((error) => addToast("warning", error.message))} title={item.pinned ? "Unpin" : "Pin"}>◇</button>
                        <button onClick={() => void copyItem(item)} title="Copy">⧉</button>
                        <button onClick={() => void deleteItem(item).catch((error) => addToast("error", error.message))} title="Move to trash">⌫</button>
                      </>}
                    </div>
                  </article>
                ))}
              </section>
            </>
          ) : section === "dashboard" ? (
            <Dashboard stats={stats} maxType={maxType} greeting={greeting} />
          ) : section === "activity" ? (
            <ActivityView activity={activity} />
          ) : (
            <SettingsView retention={retention} setRetention={setRetention} addToast={addToast} />
          )}
        </div>
      </main>

      {expanded && <ExpandedView item={expanded} onClose={() => setExpanded(null)} onCopy={copyItem} onTags={async (tags) => { await mutateItem(expanded.id, { tags }); setExpanded((current) => current ? { ...current, tags } : null); }} />}
      {shortcutsOpen && <Shortcuts onClose={() => setShortcutsOpen(false)} />}
      <div className="toast-stack">
        {toasts.map((toast) => <button key={toast.id} className={`toast ${toast.type}`} role="alert" onClick={() => setToasts((current) => current.filter((entry) => entry.id !== toast.id))}><span>{toast.type === "success" ? "✓" : toast.type === "error" ? "!" : toast.type === "warning" ? "△" : "i"}</span>{toast.message}<b>×</b></button>)}
      </div>
    </div>
  );
}

function SkeletonCard() {
  return <div className="skeleton-card"><i /><span /><b /></div>;
}

function EmptyState({ trash, onRead }: { trash: boolean; onRead: () => void }) {
  return <div className="empty-state"><div className="empty-glyph"><span>[</span><b>≡</b><span>]</span></div><h2>{trash ? "Trash is empty" : "Your clipboard is empty"}</h2><p>{trash ? "Deleted items will wait here for 30 days." : "Press Ctrl+C, then let Clipboard Vault collect it."}</p>{!trash && <button onClick={onRead}>Read clipboard now</button>}</div>;
}

function ExpandedView({ item, onClose, onCopy, onTags }: { item: VaultItem; onClose: () => void; onCopy: (item: VaultItem) => Promise<void>; onTags: (tags: string[]) => Promise<void> }) {
  const [tagText, setTagText] = useState(item.tags.join(", "));
  return <div className="modal-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}><section className="expanded-view" role="dialog" aria-modal="true" aria-label="Clipboard item"><header><span className={`type-icon ${item.type}`}>{TYPE_META[item.type].glyph}</span><div><b>{TYPE_META[item.type].label} clip</b><small>{new Date(item.createdAt).toLocaleString()} · {formatBytes(item.sizeBytes)}</small></div><button onClick={onClose} aria-label="Close">×</button></header><div className={`payload-view ${item.type}`}>{item.type === "image" ? <img src={item.payload} alt="Clipboard content" /> : item.type === "html" ? <iframe sandbox="" srcDoc={item.payload} title="Sandboxed HTML preview" /> : <pre><code>{item.payload}</code></pre>}</div><label className="tag-editor"><span>Tags</span><input value={tagText} onChange={(event) => setTagText(event.target.value)} placeholder="work, reference" onBlur={() => void onTags(tagText.split(",").map((tag) => tag.trim()).filter(Boolean))} /></label><footer><button className="secondary" onClick={onClose}>Close</button><button className="primary" onClick={() => void onCopy(item)}>⧉ Copy to clipboard</button></footer></section></div>;
}

function Dashboard({ stats, maxType, greeting }: { stats: Stats | null; maxType: number; greeting: string }) {
  const types = Object.entries(stats?.type_breakdown ?? {});
  const maxDaily = Math.max(1, ...(stats?.daily_counts ?? []).map((entry) => entry.count));
  return <><div className="page-heading dashboard-heading"><div><p className="eyebrow">VAULT OVERVIEW</p><h1>Your clipboard at a glance.</h1><p>A quiet pulse check, {greeting}. No surveillance, just your own useful trail.</p></div><a className="export-button" href="/items/export?format=json">⇩ Export vault</a></div><div className="stat-grid"><StatCard label="Stored clips" value={String(stats?.total_items ?? 0)} note="Active items" accent="lime" glyph="⌁" /><StatCard label="Vault size" value={formatBytes(stats?.total_size_bytes ?? 0)} note={`${formatBytes(stats?.avg_payload_size ?? 0)} average`} accent="amber" glyph="◫" /><StatCard label="Pinned" value={String(stats?.pinned_items ?? 0)} note="Up to 10" accent="cream" glyph="◇" /></div><div className="dashboard-grid"><section className="panel"><div className="panel-title"><div><p className="eyebrow">FORMAT MIX</p><h2>What you collect</h2></div><span>{stats?.total_items ?? 0} total</span></div><div className="type-breakdown">{types.length ? types.map(([type, count]) => <div key={type}><span className={`type-icon ${type}`}>{TYPE_META[type as ItemType]?.glyph ?? "·"}</span><b>{TYPE_META[type as ItemType]?.label ?? type}</b><div><i style={{ width: `${(count / maxType) * 100}%` }} /></div><strong>{count}</strong></div>) : <p className="muted">Collect a few clips to see the mix.</p>}</div></section><section className="panel"><div className="panel-title"><div><p className="eyebrow">LAST 30 DAYS</p><h2>Capture rhythm</h2></div></div><div className="daily-chart" aria-label="Daily clip count chart">{(stats?.daily_counts ?? []).slice(0, 14).reverse().map((entry) => <div key={entry.date} title={`${entry.date}: ${entry.count}`}><i style={{ height: `${Math.max(8, (entry.count / maxDaily) * 100)}%` }} /><small>{new Date(`${entry.date}T00:00:00`).toLocaleDateString(undefined, { weekday: "narrow" })}</small></div>)}{!stats?.daily_counts.length && <p className="muted">Your rhythm will appear here.</p>}</div></section></div></>;
}

function StatCard({ label, value, note, accent, glyph }: { label: string; value: string; note: string; accent: string; glyph: string }) {
  return <article className={`stat-card ${accent}`}><span>{glyph}</span><p>{label}</p><strong>{value}</strong><small>{note}</small></article>;
}

function ActivityView({ activity }: { activity: Activity[] }) {
  return <><div className="page-heading"><div><p className="eyebrow">AUDIT TRAIL</p><h1>Recent activity</h1><p>The last 200 changes made inside your vault.</p></div></div><section className="activity-panel"><header><b>Action</b><b>Item</b><b>Time</b><b>Origin</b></header>{activity.length ? activity.map((entry) => <div key={entry.id}><span className={`activity-icon ${entry.action}`}>{entry.action.includes("delete") ? "⌫" : entry.action === "create" ? "+" : entry.action === "export" ? "⇩" : "↺"}</span><b>{entry.action.replaceAll("_", " ")}</b><code>{entry.itemId?.slice(0, 8) ?? "—"}</code><time>{timeAgo(entry.createdAt)}</time><small>{entry.ip ?? "unknown"}</small></div>) : <div className="activity-empty">No activity yet. Your first saved clip will appear here.</div>}</section></>;
}

function SettingsView({ retention, setRetention, addToast }: { retention: { max_items: number; max_age_days: number; max_total_size_mb: number; action_on_limit: string }; setRetention: (value: typeof retention) => void; addToast: (type: Toast["type"], message: string) => void }) {
  const save = async () => { const response = await fetch("/settings", { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify(retention) }); if (response.ok) addToast("success", "Retention policy saved"); else addToast("error", "Could not save settings"); };
  return <><div className="page-heading"><div><p className="eyebrow">VAULT POLICY</p><h1>Keep what matters.</h1><p>Set boundaries that keep your clipboard useful, lean, and private.</p></div></div><section className="settings-panel"><div className="settings-copy"><p className="eyebrow">RETENTION LIMITS</p><h2>Automatic housekeeping</h2><p>When any limit is reached, Clipboard Vault follows the action you choose below. Deleted items remain recoverable for 30 days.</p></div><div className="settings-form"><label><span>Maximum items</span><input type="number" value={retention.max_items} onChange={(event) => setRetention({ ...retention, max_items: Number(event.target.value) })} /><small>100–100,000 items</small></label><label><span>Maximum age</span><div className="input-suffix"><input type="number" value={retention.max_age_days} onChange={(event) => setRetention({ ...retention, max_age_days: Number(event.target.value) })} /><b>days</b></div></label><label><span>Maximum storage</span><div className="input-suffix"><input type="number" value={retention.max_total_size_mb} onChange={(event) => setRetention({ ...retention, max_total_size_mb: Number(event.target.value) })} /><b>MB</b></div></label><label><span>When a limit is reached</span><select value={retention.action_on_limit} onChange={(event) => setRetention({ ...retention, action_on_limit: event.target.value })}><option value="delete_oldest">Delete oldest first</option><option value="reject_new">Reject new clips</option></select></label><button className="primary save-settings" onClick={() => void save()}>Save policy</button></div></section><section className="privacy-note"><span>◎</span><div><b>Private by design</b><p>Your clipboard is read only while this page is active and permission is granted. Duplicates are hashed before upload; capture pauses when you leave the tab.</p></div></section></>;
}

function Shortcuts({ onClose }: { onClose: () => void }) {
  return <div className="modal-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}><section className="shortcuts-modal" role="dialog" aria-modal="true"><header><div><p className="eyebrow">MOVE FASTER</p><h2>Keyboard shortcuts</h2></div><button onClick={onClose}>×</button></header>{[["⌘ / Ctrl + F", "Focus search"], ["⌘ / Ctrl + A", "Select visible clips"], ["Delete", "Move selection to trash"], ["Escape", "Close or clear"], ["?", "Toggle this guide"]].map(([key, value]) => <div key={key}><kbd>{key}</kbd><span>{value}</span></div>)}</section></div>;
}
