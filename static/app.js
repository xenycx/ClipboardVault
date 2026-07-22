(() => {
  "use strict";

  const MiB = 1024 * 1024;
  const TUS_VERSION = "1.0.0";
  const CHUNK_SIZE = 16 * MiB;
  const RETRY_DELAYS = [1000, 2500, 5000, 10000];
  const RESUME_KEY = "clipboard-vault.uploads.v1";
  const TEXT_LIMIT = 10 * MiB;
  const HIGHLIGHT_LIMIT = MiB;
  const IMAGE_LIMIT = 50 * MiB;

  const themeToggle = document.querySelector("[data-theme-toggle]");
  const setTheme = (theme) => {
    const nextTheme = theme === "light" ? "light" : "dark";
    document.documentElement.dataset.theme = nextTheme;
    try { localStorage.setItem("clipboard-vault.theme", nextTheme); } catch {}
    if (themeToggle) {
      const isLight = nextTheme === "light";
      themeToggle.setAttribute("aria-label", `Switch to ${isLight ? "dark" : "light"} theme`);
      themeToggle.setAttribute("aria-pressed", String(isLight));
    }
  };
  if (themeToggle) {
    setTheme(document.documentElement.dataset.theme);
    themeToggle.addEventListener("click", () => {
      setTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light");
    });
  }

  const errorBox = document.querySelector("[data-auth-error]");
  const showError = (message) => { if (errorBox) errorBox.textContent = message || "Something went wrong."; };
  const authParameters = new URLSearchParams(window.location.search);
  const requestedReturn = authParameters.get("returnTo") || "/pending";
  const returnTo = requestedReturn.startsWith("/") && !requestedReturn.startsWith("//") ? requestedReturn : "/pending";
  const authRequest = async (path, body) => {
    const response = await fetch(path, {
      method: "POST", credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(apiMessage(data) || "Request failed");
    return data;
  };

  document.querySelectorAll("[data-auth-form]").forEach((form) => {
    form.addEventListener("submit", async (event) => {
      event.preventDefault(); showError("");
      const values = Object.fromEntries(new FormData(form));
      const action = form.dataset.authForm === "signup" ? "sign-up/email" : "sign-in/email";
      try { await authRequest("/api/auth/" + action, values); window.location.assign(returnTo); }
      catch (error) { showError(error.message); }
    });
  });
  const oauthErrorMessages = {
    email_not_found: "This provider did not return a verified email address. Add one there or use another sign-in method.",
    email_is_missing: "This provider did not return a verified email address. Add one there or use another sign-in method.",
    unable_to_get_user_info: "This provider did not return a verified email address. Add one there or use another sign-in method.",
    email_not_verified: "This provider did not return a verified email address. Add one there or use another sign-in method.",
    account_not_linked: "This identity could not be linked safely. Sign in with the method already attached to your account.",
    unable_to_link_account: "This identity could not be linked safely. Sign in with the method already attached to your account.",
    account_already_linked_to_different_user: "That social account is already linked to another user.",
    access_denied: "Social sign-in was cancelled or denied.",
  };
  const oauthError = authParameters.get("error") || authParameters.get("oauthError");
  if (oauthError) {
    showError(oauthErrorMessages[oauthError] || "Social sign-in could not be completed. Please try again or use email and password.");
    authParameters.delete("error");
    authParameters.delete("oauthError");
    authParameters.delete("error_description");
    const cleanQuery = authParameters.toString();
    window.history.replaceState({}, "", window.location.pathname + (cleanQuery ? `?${cleanQuery}` : ""));
  }

  const socialProviders = document.querySelector("[data-social-providers]");
  const socialProviderTemplate = document.querySelector("[data-social-provider-template]");
  const socialDivider = document.querySelector("[data-social-divider]");
  const startSocialSignIn = async (button, provider) => {
    showError("");
    button.disabled = true;
    try {
      const errorCallbackURL = `/login?returnTo=${encodeURIComponent(returnTo)}`;
      const data = await authRequest("/api/auth/sign-in/social", {
        provider,
        callbackURL: returnTo,
        errorCallbackURL,
      });
      if (!data.url) throw new Error("The provider did not return a sign-in URL");
      window.location.assign(data.url);
    } catch (error) {
      button.disabled = false;
      showError(error.message);
    }
  };
  if (socialProviders && socialProviderTemplate) {
    fetch("/api/auth/vault/providers", {
      credentials: "same-origin",
      headers: { accept: "application/json" },
      cache: "no-store",
    }).then(async (response) => {
      if (!response.ok) throw new Error("Could not load social providers");
      return response.json();
    }).then((payload) => {
      const providers = Array.isArray(payload?.data?.providers) ? payload.data.providers : [];
      providers.forEach((provider) => {
        if (!provider || !/^[a-z0-9-]+$/.test(provider.id) || typeof provider.label !== "string") return;
        const fragment = socialProviderTemplate.content.cloneNode(true);
        const button = fragment.querySelector("button");
        if (!button) return;
        button.dataset.socialProvider = provider.id;
        button.querySelector("[data-provider-mark]").textContent = provider.label.slice(0, 1).toUpperCase();
        button.querySelector("[data-provider-label]").textContent = `Continue with ${provider.label}`;
        button.addEventListener("click", () => startSocialSignIn(button, provider.id));
        socialProviders.append(fragment);
      });
      if (socialProviders.children.length) {
        socialProviders.hidden = false;
        if (socialDivider) socialDivider.hidden = false;
      }
    }).catch(() => {
      // Email/password remains available when provider discovery is unavailable.
    });
  }
  document.querySelectorAll("[data-signout]").forEach((button) => {
    button.addEventListener("click", async () => {
      await authRequest("/api/auth/sign-out", {}); window.location.assign("/login");
    });
  });
  const bootstrap = document.querySelector("[data-bootstrap]");
  if (bootstrap) bootstrap.addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      const token = new FormData(event.currentTarget).get("token");
      await authRequest("/api/auth/vault/bootstrap", { token }); window.location.assign("/");
    } catch (error) { showError(error.message); }
  });
  const reset = document.querySelector("[data-reset-password]");
  if (reset) reset.addEventListener("submit", async (event) => {
    event.preventDefault();
    try {
      await authRequest("/api/auth/reset-password", Object.fromEntries(new FormData(event.currentTarget)));
      window.location.assign("/login");
    } catch (error) { showError(error.message); }
  });
  document.querySelectorAll("[data-copy-value]").forEach((button) => {
    button.addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(button.dataset.copyValue || ""); toast("Copied"); }
      catch { toast("Your browser blocked copying"); }
    });
  });
  document.querySelectorAll("[data-delete-id]").forEach((button) => {
    button.addEventListener("click", async () => {
      if (!window.confirm("Move this item to trash?")) return;
      const response = await fetch("/api/v1/items/" + encodeURIComponent(button.dataset.deleteId), { method: "DELETE", credentials: "same-origin" });
      if (response.ok) button.closest("[data-item-card]")?.remove(); else toast("Delete failed");
    });
  });
  const filter = document.querySelector("[data-item-filter]");
  if (filter) filter.addEventListener("input", () => {
    const term = filter.value.trim().toLowerCase();
    document.querySelectorAll("[data-item-card]").forEach((card) => {
      card.hidden = Boolean(term && !(card.dataset.search || card.textContent).toLowerCase().includes(term));
    });
  });

  class ResumableUpload {
    constructor(form) {
      this.form = form;
      this.fileInput = form.querySelector("input[type=file]");
      this.progress = form.querySelector("[data-upload-progress]");
      this.status = form.querySelector("[data-upload-status]");
      this.controls = form.querySelector("[data-upload-controls]");
      this.submit = form.querySelector("[data-upload-start]");
      this.xhr = null;
      this.file = null;
      this.url = "";
      this.offset = 0;
      this.fingerprint = "";
      this.paused = false;
      this.cancelled = false;
      this.running = false;
      this.retryIndex = 0;
      this.startedAt = 0;
      this.startedOffset = 0;
      this.chunkSize = CHUNK_SIZE;
      form.addEventListener("submit", (event) => { event.preventDefault(); this.start(); });
      form.querySelector("[data-upload-pause]")?.addEventListener("click", () => this.pause());
      form.querySelector("[data-upload-resume]")?.addEventListener("click", () => this.resume());
      form.querySelector("[data-upload-retry]")?.addEventListener("click", () => this.resume());
      form.querySelector("[data-upload-cancel]")?.addEventListener("click", () => this.cancel());
      this.fileInput.addEventListener("change", () => {
        if (!this.running) {
          this.file = null; this.url = ""; this.offset = 0; this.fingerprint = "";
        }
      });
    }

    async start() {
      if (this.running) return;
      this.file = this.fileInput.files?.[0];
      if (!this.file) return toast("Choose a file first");
      const maximum = Number(this.form.dataset.maxUploadBytes || 0) || Number(this.form.dataset.maxUploadMb || 0) * MiB;
      if (maximum && this.file.size > maximum) return toast(`This file exceeds the ${formatBytes(maximum)} workspace limit`);
      this.resetState();
      this.running = true;
      this.submit.disabled = true;
      this.fileInput.disabled = true;
      this.setState("Preparing resumable upload…", "working");
      try {
        if (!window.crypto?.subtle) {
          if (this.file.size <= 100 * MiB) return await this.legacyUpload();
          throw new Error("Resumable uploads require HTTPS in this browser");
        }
        await this.discoverChunkSize();
        this.fingerprint = await fingerprint(this.file);
        const saved = loadResumes().find((entry) => entry.fingerprint === this.fingerprint && entry.size === this.file.size);
        if (saved?.url) {
          this.url = safeSameOriginUrl(saved.url);
          if (this.url) await this.readOffset();
        }
        if (!this.url && await this.create()) return;
        this.startedAt = performance.now();
        this.startedOffset = this.offset;
        this.showControls("uploading");
        await this.sendNext();
      } catch (error) {
        this.fail(error);
      }
    }

    resetState() {
      this.paused = false; this.cancelled = false; this.retryIndex = 0; this.offset = 0; this.url = "";
      this.progress.hidden = false; this.progress.value = 0;
      this.controls.hidden = true;
    }

    async create() {
      const values = new FormData(this.form);
      const metadata = {
        filename: truncateUtf8(this.file.name, 512),
        virtualPath: truncateUtf8(String(values.get("virtual_path") || "/"), 1024),
        tags: truncateUtf8(String(values.get("tags") || ""), 3072),
        contentType: truncateUtf8(String(this.file.type || ""), 255),
      };
      const response = await fetch("/api/v1/uploads", {
        method: "POST", credentials: "same-origin",
        headers: {
          "Tus-Resumable": TUS_VERSION,
          "Upload-Length": String(this.file.size),
          "Upload-Metadata": encodeTusMetadata(metadata),
        },
      });
      if (!response.ok) {
        if ((response.status === 404 || response.status === 405) && this.file.size <= 100 * MiB) { await this.legacyUpload(); return true; }
        throw new Error(await responseMessage(response, "Could not create upload"));
      }
      const location = response.headers.get("Location");
      this.url = safeSameOriginUrl(location);
      if (!this.url) throw new Error("The server returned an invalid upload location");
      saveResume({ fingerprint: this.fingerprint, url: this.url, name: this.file.name, size: this.file.size, lastModified: this.file.lastModified });
    }

    async discoverChunkSize() {
      try {
        const response = await fetch("/api/v1/uploads", { method: "OPTIONS", credentials: "same-origin" });
        const advertised = Number(response.headers.get("X-Tus-Chunk-Size"));
        if (response.ok && Number.isSafeInteger(advertised) && advertised > 0) {
          this.chunkSize = Math.min(advertised, CHUNK_SIZE);
        }
      } catch {}
    }

    legacyUpload() {
      this.setState("This server does not support resumable uploads yet; using the compatible uploader…", "working");
      return new Promise((resolve, reject) => {
        const xhr = this.xhr = new XMLHttpRequest();
        xhr.open("POST", this.form.action);
        xhr.withCredentials = true;
        xhr.upload.addEventListener("progress", (event) => {
          if (event.lengthComputable) this.updateProgress(event.loaded, event.total);
        });
        xhr.addEventListener("load", () => xhr.status >= 200 && xhr.status < 300 ? (window.location.reload(), resolve()) : reject(new Error(xhrMessage(xhr))));
        xhr.addEventListener("error", () => reject(new Error("Upload interrupted")));
        xhr.send(new FormData(this.form));
      });
    }

    async readOffset() {
      const response = await fetch(this.url, { method: "HEAD", credentials: "same-origin", headers: { "Tus-Resumable": TUS_VERSION } });
      if (response.status === 404 || response.status === 410) { removeResume(this.fingerprint); this.url = ""; this.offset = 0; return; }
      if (!response.ok) throw new Error(await responseMessage(response, "Could not resume upload"));
      this.offset = strictOffset(response.headers.get("Upload-Offset"), this.file.size);
    }

    async sendNext() {
      if (this.cancelled || this.paused || !this.running) return;
      if (this.offset >= this.file.size) return this.pollFinalization();
      const start = this.offset;
      const end = Math.min(start + this.chunkSize, this.file.size);
      this.setState(`Uploading ${formatBytes(start)} of ${formatBytes(this.file.size)}`, "uploading");
      try {
        const nextOffset = await this.patchChunk(this.file.slice(start, end), start);
        if (this.paused || this.cancelled) return;
        this.offset = nextOffset;
        this.retryIndex = 0;
        this.updateProgress(this.offset, this.file.size);
        await this.sendNext();
      } catch (error) {
        if (this.paused || this.cancelled || error.name === "AbortError") return;
        await this.recover(error);
      }
    }

    patchChunk(blob, offset) {
      return new Promise((resolve, reject) => {
        const xhr = this.xhr = new XMLHttpRequest();
        xhr.open("PATCH", this.url);
        xhr.withCredentials = true;
        xhr.setRequestHeader("Tus-Resumable", TUS_VERSION);
        xhr.setRequestHeader("Upload-Offset", String(offset));
        xhr.setRequestHeader("Content-Type", "application/offset+octet-stream");
        xhr.upload.addEventListener("progress", (event) => {
          if (event.lengthComputable) this.updateProgress(offset + event.loaded, this.file.size);
        });
        xhr.addEventListener("load", () => {
          this.xhr = null;
          if (xhr.status >= 200 && xhr.status < 300) {
            try { resolve(strictOffset(xhr.getResponseHeader("Upload-Offset"), this.file.size)); }
            catch (error) { reject(error); }
          } else reject(new Error(xhrMessage(xhr)));
        });
        xhr.addEventListener("error", () => { this.xhr = null; reject(new Error("Network connection lost")); });
        xhr.addEventListener("abort", () => { this.xhr = null; const error = new Error("Upload paused"); error.name = "AbortError"; reject(error); });
        xhr.send(blob);
      });
    }

    async recover(originalError) {
      try { await this.readOffset(); } catch {}
      if (!this.url) return this.fail(originalError);
      if (this.retryIndex >= RETRY_DELAYS.length) return this.fail(originalError);
      const delay = RETRY_DELAYS[this.retryIndex++];
      this.setState(`Connection interrupted. Retrying in ${Math.ceil(delay / 1000)}s…`, "warning");
      await wait(delay);
      return this.sendNext();
    }

    pause() {
      if (!this.running || this.paused) return;
      this.paused = true; this.xhr?.abort();
      this.setState(`Paused at ${formatBytes(this.offset)}. You can safely resume.`, "warning");
      this.showControls("paused");
    }

    async resume() {
      if (!this.file) return this.start();
      if (!this.url) { this.running = false; return this.start(); }
      this.paused = false; this.cancelled = false; this.running = true; this.retryIndex = 0;
      this.startedAt = performance.now(); this.startedOffset = this.offset;
      this.showControls("uploading");
      try { await this.readOffset(); await this.sendNext(); } catch (error) { this.fail(error); }
    }

    async cancel() {
      if (!this.url || !window.confirm("Cancel this upload and remove its uploaded chunks?")) return;
      this.cancelled = true; this.running = false; this.xhr?.abort();
      try {
        const response = await fetch(this.url, { method: "DELETE", credentials: "same-origin", headers: { "Tus-Resumable": TUS_VERSION } });
        if (!response.ok && response.status !== 404 && response.status !== 410) throw new Error(await responseMessage(response, "Cancel failed"));
        removeResume(this.fingerprint); this.setState("Upload cancelled.", "warning"); this.showControls("done");
      } catch (error) { this.fail(error); }
    }

    async pollFinalization() {
      this.xhr = null; this.running = false; this.progress.value = 100; this.showControls("finalizing");
      this.setState("Upload received. Verifying and saving…", "working");
      const statusUrl = this.url.replace(/\/$/, "") + "/status";
      for (;;) {
        const response = await fetch(statusUrl, { credentials: "same-origin", headers: { "Accept": "application/json" } });
        const data = await response.json().catch(() => ({}));
        if (!response.ok) throw new Error(apiMessage(data) || "Could not check upload status");
        const state = String(data.state || data.status || "").toLowerCase();
        if (["completed", "complete", "saved"].includes(state) || data.itemId || data.item_id) {
          removeResume(this.fingerprint); this.setState("Saved to your vault.", "success");
          window.setTimeout(() => window.location.reload(), 500); return;
        }
        if (["failed", "cancelled", "canceled", "expired"].includes(state)) throw new Error(data.error || data.message || `Upload ${state}`);
        await wait(1200);
      }
    }

    updateProgress(loaded, total) {
      const percent = total ? Math.min(100, loaded / total * 100) : 0;
      this.progress.value = percent;
      const elapsed = Math.max((performance.now() - this.startedAt) / 1000, .1);
      const speed = Math.max(0, loaded - this.startedOffset) / elapsed;
      const eta = speed > 0 ? (total - loaded) / speed : 0;
      this.setState(`${formatBytes(loaded)} of ${formatBytes(total)} · ${percent.toFixed(1)}% · ${formatBytes(speed)}/s${eta ? ` · ${formatDuration(eta)} left` : ""}`, "uploading");
    }

    setState(message, state) { this.status.textContent = message; this.status.dataset.state = state; }
    showControls(state) {
      this.controls.hidden = state === "done" || state === "finalizing";
      this.form.querySelector("[data-upload-pause]").hidden = state !== "uploading";
      this.form.querySelector("[data-upload-resume]").hidden = state !== "paused";
      this.form.querySelector("[data-upload-retry]").hidden = state !== "failed";
      this.form.querySelector("[data-upload-cancel]").hidden = state === "done" || state === "finalizing" || !this.url;
      this.submit.disabled = !["done", "failed"].includes(state);
      this.fileInput.disabled = !["done", "failed"].includes(state);
    }
    fail(error) {
      this.running = false; this.setState(error?.message || "Upload failed", "error"); this.showControls("failed"); toast(error?.message || "Upload failed");
    }
  }

  document.querySelectorAll("[data-upload-form]").forEach((form) => new ResumableUpload(form));
  const fileInput = document.querySelector("[data-dropzone] input[type=file]");
  if (fileInput) fileInput.addEventListener("change", () => {
    const file = fileInput.files?.[0];
    const label = document.querySelector("[data-dropzone] strong");
    if (file && label) label.textContent = `${file.name} (${formatBytes(file.size)})`;
  });

  const preview = document.querySelector("[data-preview-dialog]");
  if (preview) setupPreview(preview);
  const storageWarning = document.querySelector("[data-storage-warning]");
  if (storageWarning) fetch("/api/v1/storage", { credentials: "same-origin", headers: { "Accept": "application/json" } })
    .then((response) => response.ok ? response.json() : null)
    .then((status) => { if (status && (status.lowStorage || status.low_storage)) storageWarning.hidden = false; })
    .catch(() => {});
  const purgeForm = document.querySelector("[data-storage-purge]");
  if (purgeForm) purgeForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const values = new FormData(purgeForm);
    const itemIds = values.getAll("item_ids").map(String);
    if (!itemIds.length) return toast("Select at least one file");
    const submit = purgeForm.querySelector('button[type="submit"]'); submit.disabled = true;
    try {
      const response = await fetch(purgeForm.action, { method: "POST", credentials: "same-origin", body: new URLSearchParams(values) });
      if (!response.ok) throw new Error(await responseMessage(response, "Cleanup failed"));
      toast("Selected files were permanently removed"); window.setTimeout(() => window.location.assign(response.url || "/storage"), 400);
    } catch (error) { toast(error.message || "Cleanup failed"); submit.disabled = false; }
  });

  function setupPreview(dialog) {
    const title = dialog.querySelector("[data-preview-title]");
    const meta = dialog.querySelector("[data-preview-meta]");
    const body = dialog.querySelector("[data-preview-body]");
    const download = dialog.querySelector("[data-preview-download]");
    const tabs = dialog.querySelector("[data-preview-tabs]");
    let opener = null;
    let objectUrl = "";
    let controller = null;

    const close = () => {
      controller?.abort(); controller = null;
      if (objectUrl) URL.revokeObjectURL(objectUrl); objectUrl = "";
      dialog.close(); body.replaceChildren(); tabs.hidden = true; opener?.focus();
    };
    dialog.querySelectorAll("[data-preview-close]").forEach((button) => button.addEventListener("click", close));
    dialog.addEventListener("cancel", (event) => { event.preventDefault(); close(); });
    dialog.addEventListener("click", (event) => { if (event.target === dialog) close(); });

    document.querySelectorAll("[data-preview-id]").forEach((button) => button.addEventListener("click", async () => {
      opener = button; controller?.abort(); controller = new AbortController();
      const id = encodeURIComponent(button.dataset.previewId);
      const url = `/api/v1/items/${id}/preview`;
      title.textContent = button.dataset.previewName || "File preview";
      meta.textContent = `${button.dataset.previewKind || "file"} · ${formatBytes(Number(button.dataset.previewSize || 0))}`;
      download.href = `/api/v1/items/${id}/content?download=1`;
      body.replaceChildren(messageNode("Loading preview…")); tabs.hidden = true;
      dialog.showModal(); dialog.querySelector("[data-preview-close]")?.focus();
      try {
        const head = await fetch(url, { method: "HEAD", credentials: "same-origin", signal: controller.signal });
        if (!head.ok) throw new Error(await responseMessage(head, "Preview unavailable"));
        const kind = classifyPreview(head.headers, button.dataset.previewKind);
        const responseSize = Number(head.headers.get("Content-Length") || 0);
        const size = Number(button.dataset.previewSize || responseSize);
        meta.textContent = `${kind.label} · ${formatBytes(size)}`;
        if (kind.type === "image") {
          if (size > IMAGE_LIMIT) throw new Error(`Images larger than ${formatBytes(IMAGE_LIMIT)} are download-only`);
          const image = document.createElement("img"); image.className = "preview-image"; image.alt = title.textContent;
          image.addEventListener("error", () => body.replaceChildren(messageNode("Your browser could not display this image.")));
          image.src = url; body.replaceChildren(image); return;
        }
        if (kind.type === "pdf") {
          const frame = document.createElement("iframe"); frame.className = "preview-pdf"; frame.title = title.textContent; frame.src = url;
          body.replaceChildren(frame); return;
        }
        if (kind.type !== "text") throw new Error("This file type is download-only");
        const response = await fetch(url, { credentials: "same-origin", signal: controller.signal });
        if (!response.ok) throw new Error(await responseMessage(response, "Preview unavailable"));
        const blob = await response.blob();
        const text = decodeText(await blob.arrayBuffer(), response.headers.get("X-Preview-Encoding"));
        const truncated = response.headers.get("X-Preview-Truncated") === "true" || Number(button.dataset.previewSize || 0) > TEXT_LIMIT;
        renderTextPreview(body, tabs, text, kind, truncated, blob.size <= HIGHLIGHT_LIMIT);
      } catch (error) {
        if (error.name !== "AbortError") body.replaceChildren(messageNode(error.message || "Preview unavailable", true));
      }
    }));
  }

  function renderTextPreview(body, tabs, source, kind, truncated, highlight) {
    const sourcePanel = document.createElement("pre"); sourcePanel.className = "preview-source";
    if (kind.format === "text" || kind.format === "markdown") sourcePanel.classList.add("wrap");
    const code = document.createElement("code"); code.textContent = source; sourcePanel.append(code);
    if (highlight && window.hljs) {
      try { window.hljs.highlightElement(code); } catch {}
    }
    const notice = truncated ? messageNode(`Showing the first ${formatBytes(TEXT_LIMIT)}. Download to read the complete file.`) : null;
    const canRenderMarkdown = kind.format === "markdown" && typeof window.marked?.parse === "function" && typeof window.DOMPurify?.sanitize === "function";
    const canRenderHtml = kind.format === "html" && typeof window.DOMPurify?.sanitize === "function";
    if (!canRenderMarkdown && !canRenderHtml) {
      body.replaceChildren(...(notice ? [notice, sourcePanel] : [sourcePanel])); return;
    }
    const rendered = document.createElement("article"); rendered.className = "preview-rendered";
    const unsafe = canRenderMarkdown ? window.marked.parse(source) : source;
    rendered.innerHTML = window.DOMPurify.sanitize(unsafe, {
      USE_PROFILES: { html: true }, FORBID_TAGS: ["style", "form", "iframe", "object", "embed", "script", "img", "picture", "source", "audio", "video", "track", "link", "input", "meta"],
      FORBID_ATTR: ["style", "srcdoc"], ALLOW_UNKNOWN_PROTOCOLS: false,
    });
    rendered.querySelectorAll("a").forEach((link) => { link.target = "_blank"; link.rel = "noopener noreferrer nofollow"; });
    const switchTab = (name) => {
      const showSource = name === "source";
      sourcePanel.hidden = !showSource; rendered.hidden = showSource;
      tabs.querySelectorAll("button").forEach((button) => button.setAttribute("aria-selected", String(button.dataset.previewTab === name)));
    };
    tabs.hidden = false; tabs.querySelectorAll("[data-preview-tab]").forEach((button) => button.onclick = () => switchTab(button.dataset.previewTab));
    body.replaceChildren(...(notice ? [notice, rendered, sourcePanel] : [rendered, sourcePanel])); switchTab("rendered");
  }

  function classifyPreview(headers, fallback) {
    const hint = (headers.get("X-Preview-Kind") || fallback || "").toLowerCase();
    const mime = (headers.get("Content-Type") || "").split(";", 1)[0].toLowerCase();
    if (hint === "image" || (mime.startsWith("image/") && mime !== "image/svg+xml")) return { type: "image", label: "Image" };
    if (hint === "pdf" || mime === "application/pdf") return { type: "pdf", label: "PDF" };
    if (hint === "markdown" || /markdown/.test(mime)) return { type: "text", format: "markdown", label: "Markdown" };
    if (hint === "html" || mime === "text/html") return { type: "text", format: "html", label: "HTML" };
    if (hint === "text" || hint === "code" || mime.startsWith("text/") || /json|xml|yaml|javascript/.test(mime)) return { type: "text", format: hint, label: hint === "code" ? "Code" : "Text" };
    return { type: "unsupported", label: "File" };
  }

  window.addEventListener("beforeunload", (event) => {
    if (!document.querySelector('[data-upload-status][data-state="uploading"]')) return;
    event.preventDefault(); event.returnValue = "";
  });

  async function fingerprint(file) {
    const first = await file.slice(0, Math.min(MiB, file.size)).arrayBuffer();
    const lastStart = Math.max(first.byteLength, file.size - MiB);
    const last = await file.slice(lastStart, file.size).arrayBuffer();
    const identity = new TextEncoder().encode(`${file.size}:${file.lastModified}:`);
    const joined = new Uint8Array(identity.length + first.byteLength + last.byteLength);
    joined.set(identity); joined.set(new Uint8Array(first), identity.length); joined.set(new Uint8Array(last), identity.length + first.byteLength);
    const digest = await crypto.subtle.digest("SHA-256", joined);
    return Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
  }
  function encodeTusMetadata(metadata) {
    return Object.entries(metadata).filter(([, value]) => value).map(([key, value]) => `${key} ${utf8Base64(String(value))}`).join(",");
  }
  function utf8Base64(value) {
    const bytes = new TextEncoder().encode(value); let binary = "";
    bytes.forEach((byte) => { binary += String.fromCharCode(byte); }); return btoa(binary);
  }
  function truncateUtf8(value, maximumBytes) {
    const encoder = new TextEncoder();
    if (encoder.encode(value).length <= maximumBytes) return value;
    let output = "";
    for (const character of value) {
      if (encoder.encode(output + character).length > maximumBytes) break;
      output += character;
    }
    return output;
  }
  function strictOffset(value, maximum) {
    if (!/^\d+$/.test(value || "")) throw new Error("The server returned an invalid upload offset");
    const offset = Number(value); if (!Number.isSafeInteger(offset) || offset < 0 || offset > maximum) throw new Error("The server returned an invalid upload offset"); return offset;
  }
  function safeSameOriginUrl(value) {
    if (!value) return "";
    try { const url = new URL(value, window.location.href); return url.origin === window.location.origin ? url.href : ""; } catch { return ""; }
  }
  function loadResumes() {
    try { const entries = JSON.parse(localStorage.getItem(RESUME_KEY) || "[]"); return Array.isArray(entries) ? entries.slice(-20) : []; } catch { return []; }
  }
  function saveResume(entry) { try { const entries = loadResumes().filter((item) => item.fingerprint !== entry.fingerprint); entries.push(entry); localStorage.setItem(RESUME_KEY, JSON.stringify(entries.slice(-20))); } catch {} }
  function removeResume(fingerprintValue) { try { localStorage.setItem(RESUME_KEY, JSON.stringify(loadResumes().filter((item) => item.fingerprint !== fingerprintValue))); } catch {} }
  function decodeText(buffer, encoding) {
    const bytes = new Uint8Array(buffer); let charset = encoding || "utf-8";
    if (bytes[0] === 0xff && bytes[1] === 0xfe) charset = "utf-16le";
    else if (bytes[0] === 0xfe && bytes[1] === 0xff) charset = "utf-16be";
    try { return new TextDecoder(charset, { fatal: false }).decode(bytes); } catch { return new TextDecoder("utf-8").decode(bytes); }
  }
  function messageNode(message, error = false) { const node = document.createElement("p"); node.className = error ? "preview-message error" : "preview-message"; node.textContent = message; return node; }
  function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"]; const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
    return `${(bytes / 1024 ** index).toFixed(index ? 1 : 0)} ${units[index]}`;
  }
  function formatDuration(seconds) { if (seconds < 60) return `${Math.ceil(seconds)}s`; if (seconds < 3600) return `${Math.ceil(seconds / 60)}m`; return `${(seconds / 3600).toFixed(1)}h`; }
  function wait(milliseconds) { return new Promise((resolve) => window.setTimeout(resolve, milliseconds)); }
  function apiMessage(data) { return typeof data?.error === "string" ? data.error : data?.error?.message || data?.message || ""; }
  async function responseMessage(response, fallback) { const data = await response.json().catch(() => ({})); return apiMessage(data) || `${fallback} (${response.status})`; }
  function xhrMessage(xhr) { try { return apiMessage(JSON.parse(xhr.responseText)) || `Upload failed (${xhr.status})`; } catch { return `Upload failed (${xhr.status || "network error"})`; } }
  function toast(message) {
    const element = document.getElementById("toast"); if (!element) return;
    element.textContent = message; element.hidden = false; window.setTimeout(() => { element.hidden = true; }, 4000);
  }
})();
