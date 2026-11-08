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

  const $ = (selector, root = document) => root.querySelector(selector);
  const $$ = (selector, root = document) => Array.from(root.querySelectorAll(selector));
  const isTyping = (target) => Boolean(target?.closest?.("input, textarea, select, [contenteditable=''], [contenteditable='true']"));
  const anyDialogOpen = () => Boolean($("dialog[open]"));

  /* ---------- Theme ---------- */
  const themeToggles = $$("[data-theme-toggle]");
  const setTheme = (theme) => {
    const next = theme === "light" ? "light" : "dark";
    document.documentElement.dataset.theme = next;
    try { localStorage.setItem("clipboard-vault.theme", next); } catch {}
    themeToggles.forEach((button) => {
      button.setAttribute("aria-label", `Switch to ${next === "light" ? "dark" : "light"} theme`);
    });
  };
  if (themeToggles.length) {
    setTheme(document.documentElement.dataset.theme);
    themeToggles.forEach((button) => button.addEventListener("click", () => {
      setTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light");
    }));
  }

  /* ---------- Authentication ---------- */
  const errorBox = $("[data-auth-error]");
  const showError = (message) => {
    if (!errorBox) return;
    errorBox.textContent = message || "";
    errorBox.hidden = !message;
  };
  const authParameters = new URLSearchParams(window.location.search);
  const requestedReturn = authParameters.get("returnTo") || "/";
  const returnTo = requestedReturn.startsWith("/") && !requestedReturn.startsWith("//") ? requestedReturn : "/";
  const authMessages = {
    INVALID_EMAIL_OR_PASSWORD: "That email and password do not match an account.",
    USER_ALREADY_EXISTS: "An account with that email already exists. Sign in instead.",
    USER_ALREADY_EXISTS_USE_ANOTHER_EMAIL: "An account with that email already exists. Sign in instead.",
    PASSWORD_TOO_SHORT: "Use at least 8 characters for the password.",
    REGISTRATION_CLOSED: "Registration is closed. Ask the administrator to create your account.",
    BANNED_USER: "This account is disabled. Contact the administrator.",
    TOO_MANY_REQUESTS: "Too many attempts. Wait a minute and try again.",
  };
  const authRequest = async (path, body) => {
    let response;
    try {
      response = await fetch(path, {
        method: "POST", credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
    } catch {
      throw new Error("Could not reach the server. Check your connection and try again.");
    }
    const data = await response.json().catch(() => ({}));
    if (!response.ok) {
      if (response.status === 429) throw new Error(authMessages.TOO_MANY_REQUESTS);
      throw new Error(authMessages[data?.code] || apiMessage(data) || `The server refused the request (${response.status}).`);
    }
    return data;
  };
  const busy = (button, on) => {
    if (!button) return;
    button.disabled = on;
    button.classList.toggle("is-busy", on);
  };

  $$("[data-auth-form]").forEach((form) => {
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      showError("");
      const submit = $("[data-auth-submit]", form) || $("button[type=submit]", form);
      busy(submit, true);
      const values = Object.fromEntries(new FormData(form));
      const action = form.dataset.authForm === "signup" ? "sign-up/email" : "sign-in/email";
      try {
        await authRequest("/api/auth/" + action, values);
        window.location.assign(returnTo);
      } catch (error) {
        showError(error.message);
        busy(submit, false);
      }
    });
  });

  const oauthErrorMessages = {
    email_not_found: "This provider did not share a verified email address. Add one there or sign in with email.",
    email_is_missing: "This provider did not share a verified email address. Add one there or sign in with email.",
    unable_to_get_user_info: "This provider did not share a verified email address. Add one there or sign in with email.",
    email_not_verified: "This provider did not share a verified email address. Add one there or sign in with email.",
    account_not_linked: "This identity could not be linked safely. Sign in with the method already on your account.",
    unable_to_link_account: "This identity could not be linked safely. Sign in with the method already on your account.",
    account_already_linked_to_different_user: "That social account is already linked to another user.",
    unable_to_create_user: "New accounts are created by the administrator on this server.",
    access_denied: "Social sign-in was cancelled.",
  };
  const oauthError = authParameters.get("error") || authParameters.get("oauthError");
  if (oauthError && errorBox) {
    showError(oauthErrorMessages[oauthError] || "Social sign-in did not finish. Try again or use your email and password.");
    ["error", "oauthError", "error_description"].forEach((key) => authParameters.delete(key));
    const cleanQuery = authParameters.toString();
    window.history.replaceState({}, "", window.location.pathname + (cleanQuery ? `?${cleanQuery}` : ""));
  }

  const socialProviders = $("[data-social-providers]");
  const socialProviderTemplate = $("[data-social-provider-template]");
  const socialDivider = $("[data-social-divider]");
  if (socialProviders && socialProviderTemplate) {
    fetch("/api/auth/vault/providers", { credentials: "same-origin", headers: { accept: "application/json" }, cache: "no-store" })
      .then((response) => { if (!response.ok) throw new Error("providers"); return response.json(); })
      .then((payload) => {
        const providers = Array.isArray(payload?.data?.providers) ? payload.data.providers : [];
        providers.forEach((provider) => {
          if (!provider || !/^[a-z0-9-]+$/.test(provider.id) || typeof provider.label !== "string") return;
          const fragment = socialProviderTemplate.content.cloneNode(true);
          const button = $("button", fragment);
          if (!button) return;
          button.dataset.socialProvider = provider.id;
          $("[data-provider-mark]", button).textContent = provider.label.slice(0, 1).toUpperCase();
          $("[data-provider-label]", button).textContent = `Continue with ${provider.label}`;
          button.addEventListener("click", async () => {
            showError(""); busy(button, true);
            try {
              const data = await authRequest("/api/auth/sign-in/social", {
                provider: provider.id, callbackURL: returnTo, errorCallbackURL: `/login?returnTo=${encodeURIComponent(returnTo)}`,
              });
              if (!data.url) throw new Error("The provider did not return a sign-in address.");
              window.location.assign(data.url);
            } catch (error) { busy(button, false); showError(error.message); }
          });
          socialProviders.append(fragment);
        });
        if (socialProviders.children.length) {
          socialProviders.hidden = false;
          if (socialDivider) socialDivider.hidden = false;
        }
      })
      .catch(() => { /* Email and password stay available. */ });
  }

  $$("[data-signout]").forEach((button) => {
    button.addEventListener("click", async () => {
      busy(button, true);
      try { await authRequest("/api/auth/sign-out", {}); } catch {}
      window.location.assign("/login");
    });
  });
  const bootstrap = $("[data-bootstrap]");
  if (bootstrap) bootstrap.addEventListener("submit", async (event) => {
    event.preventDefault(); showError("");
    try {
      await authRequest("/api/auth/vault/bootstrap", { token: new FormData(bootstrap).get("token") });
      window.location.assign("/");
    } catch (error) { showError(error.message); }
  });
  const reset = $("[data-reset-password]");
  if (reset) reset.addEventListener("submit", async (event) => {
    event.preventDefault(); showError("");
    try {
      await authRequest("/api/auth/reset-password", Object.fromEntries(new FormData(reset)));
      window.location.assign("/login");
    } catch (error) { showError(error.message); }
  });

  /* ---------- Shared helpers: copy, confirm, forms ---------- */
  const copyText = async (value, message = "Copied") => {
    try { await navigator.clipboard.writeText(value); toast(message); }
    catch { toast("Your browser blocked copying. Select the text and copy it manually.", "error"); }
  };
  $$("[data-copy-value]").forEach((button) => {
    button.addEventListener("click", (event) => { event.stopPropagation(); copyText(button.dataset.copyValue || ""); });
  });

  const confirmDialog = $("[data-confirm]");
  const confirmAction = ({ title, text, ok = "Confirm", danger = true }) => {
    if (!confirmDialog) return Promise.resolve(window.confirm(`${title}\n\n${text || ""}`.trim()));
    $("[data-confirm-title]", confirmDialog).textContent = title;
    $("[data-confirm-text]", confirmDialog).textContent = text || "";
    const okButton = $("[data-confirm-ok]", confirmDialog);
    okButton.textContent = ok;
    okButton.classList.toggle("danger", danger);
    okButton.classList.toggle("primary", !danger);
    confirmDialog.returnValue = "";
    confirmDialog.showModal();
    return new Promise((resolve) => {
      confirmDialog.addEventListener("close", () => resolve(confirmDialog.returnValue === "ok"), { once: true });
    });
  };

  $$("form[data-confirm-submit]").forEach((form) => {
    form.addEventListener("submit", async (event) => {
      if (form.dataset.confirmed === "1") return;
      event.preventDefault();
      if (await confirmAction({ title: "Please confirm", text: form.dataset.confirmSubmit, ok: "Continue" })) {
        form.dataset.confirmed = "1";
        form.requestSubmit();
      }
    });
  });
  $$("select[data-autosubmit]").forEach((select) => {
    const fallback = $("[data-autosubmit-fallback]", select.form);
    if (fallback) fallback.hidden = true;
    select.addEventListener("change", () => select.form.requestSubmit());
  });
  // Close any open <details> menu when clicking elsewhere or pressing Escape.
  document.addEventListener("click", (event) => {
    $$("details.menu[open]").forEach((menu) => { if (!menu.contains(event.target)) menu.open = false; });
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") $$("details.menu[open]").forEach((menu) => { menu.open = false; });
  });

  /* ---------- Shell: navigation and workspace ---------- */
  const sidebar = $("[data-sidebar]");
  if (sidebar) {
    const scrim = $("[data-nav-scrim]");
    const setNav = (open) => {
      sidebar.classList.toggle("is-open", open);
      scrim?.classList.toggle("is-open", open);
    };
    $("[data-nav-open]")?.addEventListener("click", () => setNav(true));
    $("[data-nav-close]")?.addEventListener("click", () => setNav(false));
    scrim?.addEventListener("click", () => setNav(false));
    $$("a", sidebar).forEach((link) => link.addEventListener("click", () => setNav(false)));
    window.addEventListener("keydown", (event) => { if (event.key === "Escape") setNav(false); });
  }
  const collapseButton = $("[data-sidebar-collapse]");
  if (collapseButton) {
    const paintCollapse = () => {
      const collapsed = document.documentElement.dataset.sidebar === "collapsed";
      collapseButton.setAttribute("aria-expanded", String(!collapsed));
      const label = collapsed ? "Expand sidebar" : "Collapse sidebar";
      collapseButton.title = `${label} ( [ )`;
      $("span", collapseButton).textContent = label;
    };
    const toggleCollapse = () => {
      const collapsed = document.documentElement.dataset.sidebar !== "collapsed";
      if (collapsed) document.documentElement.dataset.sidebar = "collapsed";
      else delete document.documentElement.dataset.sidebar;
      try { localStorage.setItem("clipboard-vault.sidebar", collapsed ? "collapsed" : "open"); } catch {}
      paintCollapse();
    };
    paintCollapse();
    collapseButton.addEventListener("click", toggleCollapse);
    window.addEventListener("keydown", (event) => {
      if (event.key === "[" && !event.ctrlKey && !event.metaKey && !event.altKey && !isTyping(event.target) && !anyDialogOpen()) {
        event.preventDefault();
        toggleCollapse();
      }
    });
  }

  const workspaceForm = $("[data-workspace-form]");
  if (workspaceForm) {
    const select = $("[data-workspace-select]", workspaceForm);
    const avatar = $("[data-ws-avatar]");
    const paint = () => { if (avatar) avatar.textContent = (select?.selectedOptions?.[0]?.textContent || "W").trim().slice(0, 1); };
    paint();
    const submit = $("[data-workspace-submit]", workspaceForm);
    if (submit) submit.hidden = true;
    select?.addEventListener("change", () => { paint(); workspaceForm.submit(); });
  }

  /* ---------- Vault: capture ---------- */
  const slot = $("[data-slot]");
  const captureForm = $("[data-capture-form]");
  const captureInput = $("[data-capture-input]");
  const pasteMode = document.getElementById("mode-paste");
  const uploadMode = document.getElementById("mode-upload");
  captureInput?.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
      event.preventDefault();
      captureForm.requestSubmit();
    }
  });
  captureForm?.addEventListener("submit", () => busy($("button[type=submit]", captureForm), true));

  const totalCount = $("[data-total-count]");
  const adjustTotal = (delta) => {
    if (!totalCount) return;
    totalCount.textContent = String(Math.max(0, Number(totalCount.textContent || 0) + delta));
  };

  /* ---------- Vault: single-item actions ---------- */
  const removeCard = (card) => {
    if (!card) return;
    card.remove();
    adjustTotal(-1);
    selection?.sync();
  };
  const itemRequest = async (path, init, failure) => {
    const response = await fetch(path, { credentials: "same-origin", ...init });
    if (!response.ok) throw new Error(await responseMessage(response, failure));
    return response;
  };
  document.addEventListener("click", async (event) => {
    const del = event.target.closest("[data-delete-id]");
    const restore = event.target.closest("[data-restore-id]");
    const purge = event.target.closest("[data-purge-id]");
    const pin = event.target.closest("[data-pin-id]");
    try {
      if (del) {
        await itemRequest(`/api/v1/items/${encodeURIComponent(del.dataset.deleteId)}`, { method: "DELETE" }, "Could not move the item to trash");
        removeCard(del.closest("[data-item-card]"));
        toast("Moved to trash");
      } else if (restore) {
        await itemRequest(`/api/v1/items/${encodeURIComponent(restore.dataset.restoreId)}/restore`, { method: "POST" }, "Could not restore the item");
        removeCard(restore.closest("[data-item-card]"));
        toast("Restored to the vault");
      } else if (purge) {
        const ok = await confirmAction({ title: "Delete this item forever?", text: "Its file is removed from the server disk. This cannot be undone.", ok: "Delete forever" });
        if (!ok) return;
        await itemRequest(`/api/v1/items/${encodeURIComponent(purge.dataset.purgeId)}/purge`, { method: "POST" }, "Could not delete the item");
        removeCard(purge.closest("[data-item-card]"));
        toast("Deleted forever");
      } else if (pin) {
        const pinned = pin.dataset.pinned !== "true";
        await itemRequest(`/api/v1/items/${encodeURIComponent(pin.dataset.pinId)}`, {
          method: "PATCH", headers: { "content-type": "application/json" }, body: JSON.stringify({ pinned }),
        }, "Could not update the pin");
        toast(pinned ? "Pinned to the top" : "Unpinned");
        window.setTimeout(() => window.location.reload(), 300);
      }
    } catch (error) { toast(error.message, "error"); }
  });

  /* ---------- Vault: instant filter ---------- */
  const filter = $("[data-item-filter]");
  if (filter) filter.addEventListener("input", () => {
    const term = filter.value.trim().toLowerCase();
    $$("[data-item-card]").forEach((card) => {
      card.hidden = Boolean(term && !(card.dataset.search || card.textContent).toLowerCase().includes(term));
    });
  });

  /* ---------- Vault: selection and bulk actions ---------- */
  const grid = $("[data-item-grid]");
  const bar = $("[data-selection-bar]");
  const selectToggle = $("[data-select-toggle]");
  const selection = grid && bar ? (() => {
    let anchor = null;
    let selecting = false;
    const picks = () => $$("[data-pick]", grid);
    const chosen = () => picks().filter((box) => box.checked);
    const setSelecting = (on) => {
      selecting = on;
      grid.classList.toggle("is-selecting", on);
      selectToggle?.setAttribute("aria-pressed", String(on));
    };
    const sync = () => {
      const count = chosen().length;
      picks().forEach((box) => box.closest("[data-item-card]")?.classList.toggle("is-selected", box.checked));
      $("[data-selection-count]", bar).textContent = `${count} selected`;
      bar.hidden = count === 0;
      if (count > 0 && !selecting) setSelecting(true);
    };
    const clear = () => { picks().forEach((box) => { box.checked = false; }); anchor = null; setSelecting(false); sync(); };
    const selectAll = () => {
      picks().forEach((box) => { if (!box.closest("[data-item-card]")?.hidden) box.checked = true; });
      sync();
    };
    grid.addEventListener("click", (event) => {
      const box = event.target.closest("[data-pick]");
      if (box) {
        const all = picks();
        if (event.shiftKey && anchor && anchor !== box) {
          const [from, to] = [all.indexOf(anchor), all.indexOf(box)].sort((a, b) => a - b);
          all.slice(from, to + 1).forEach((other) => { if (!other.closest("[data-item-card]")?.hidden) other.checked = box.checked; });
        }
        anchor = box;
        sync();
        return;
      }
      const open = event.target.closest("[data-open]");
      if (open && selecting) {
        event.preventDefault();
        event.stopImmediatePropagation();
        const pick = $("[data-pick]", open.closest("[data-item-card]"));
        if (pick) { pick.checked = !pick.checked; anchor = pick; sync(); }
      }
    }, true);
    selectToggle?.addEventListener("click", () => { if (selecting) clear(); else setSelecting(true); });
    $("[data-select-all]", bar)?.addEventListener("click", selectAll);
    $("[data-select-clear]", bar)?.addEventListener("click", clear);
    window.addEventListener("keydown", (event) => {
      if (anyDialogOpen() || isTyping(event.target)) return;
      if (event.key === "Escape" && selecting) clear();
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a") { event.preventDefault(); setSelecting(true); selectAll(); }
    });

    const labels = {
      trash: { done: (n) => `Moved ${n} ${plural(n, "item")} to trash` },
      restore: { done: (n) => `Restored ${n} ${plural(n, "item")}` },
      purge: {
        confirm: (n) => ({ title: `Delete ${n} ${plural(n, "item")} forever?`, text: "Their files are removed from the server disk. This cannot be undone.", ok: "Delete forever" }),
        done: (n, bytes) => `Deleted ${n} ${plural(n, "item")} forever${bytes ? `, freed ${formatBytes(bytes)}` : ""}`,
      },
    };
    $$("[data-bulk]", bar).forEach((button) => button.addEventListener("click", async () => {
      const action = button.dataset.bulk;
      const ids = chosen().map((box) => box.value);
      if (!ids.length) return;
      const spec = labels[action];
      if (spec.confirm && !(await confirmAction(spec.confirm(ids.length)))) return;
      busy(button, true);
      try {
        const response = await itemRequest("/api/v1/items/bulk", {
          method: "POST", headers: { "content-type": "application/json", accept: "application/json" },
          body: JSON.stringify({ action, ids }),
        }, "The bulk action failed");
        const result = await response.json().catch(() => ({}));
        chosen().forEach((box) => { box.closest("[data-item-card]")?.remove(); adjustTotal(-1); });
        clear();
        toast(spec.done(Number(result.affected ?? ids.length), Number(result.reclaimedBytes || 0)));
        if (!$("[data-item-card]")) window.setTimeout(() => window.location.reload(), 500);
      } catch (error) { toast(error.message, "error"); }
      finally { busy(button, false); }
    }));
    return { sync, clear };
  })() : null;

  const emptyTrash = $("[data-empty-trash]");
  emptyTrash?.addEventListener("click", async () => {
    const count = Number(emptyTrash.dataset.count || 0);
    const ok = await confirmAction({
      title: "Empty the trash?",
      text: `${count} ${plural(count, "item")} will be deleted forever and their files removed from the server disk. This cannot be undone.`,
      ok: "Empty trash",
    });
    if (!ok) return;
    busy(emptyTrash, true);
    try {
      const response = await itemRequest("/api/v1/trash/empty", { method: "POST", headers: { accept: "application/json" } }, "Could not empty the trash");
      const result = await response.json().catch(() => ({}));
      toast(`Trash emptied${result.reclaimedBytes ? `, freed ${formatBytes(Number(result.reclaimedBytes))}` : ""}`);
      window.setTimeout(() => window.location.reload(), 500);
    } catch (error) { toast(error.message, "error"); busy(emptyTrash, false); }
  });

  /* ---------- Uploads (Tus resumable, sequential queue) ---------- */
  class ResumableUpload {
    constructor(form) {
      this.form = form;
      this.fileInput = $("input[type=file]", form);
      this.panel = $("[data-upload-panel]", form);
      this.nameLabel = $("[data-upload-name]", form);
      this.progress = $("[data-upload-progress]", form);
      this.status = $("[data-upload-status]", form);
      this.controls = $("[data-upload-controls]", form);
      this.submit = $("[data-upload-start]", form);
      this.queue = [];
      this.total = 0;
      this.done = 0;
      this.xhr = null; this.file = null; this.url = ""; this.offset = 0; this.fingerprint = "";
      this.paused = false; this.cancelled = false; this.running = false; this.retryIndex = 0;
      this.startedAt = 0; this.startedOffset = 0; this.chunkSize = CHUNK_SIZE;
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        const files = Array.from(this.fileInput.files || []);
        if (!files.length) return toast("Choose a file first");
        this.enqueue(files);
      });
      $("[data-upload-pause]", form)?.addEventListener("click", () => this.pause());
      $("[data-upload-resume]", form)?.addEventListener("click", () => this.resume());
      $("[data-upload-retry]", form)?.addEventListener("click", () => this.resume());
      $("[data-upload-cancel]", form)?.addEventListener("click", () => this.cancel());
    }

    get maximum() {
      return Number(this.form.dataset.maxUploadBytes || 0) || Number(this.form.dataset.maxUploadMb || 0) * MiB;
    }

    enqueue(files) {
      const accepted = files.filter((file) => {
        if (this.maximum && file.size > this.maximum) { toast(`${file.name} is larger than the ${formatBytes(this.maximum)} limit`, "error"); return false; }
        if (!file.size) { toast(`${file.name} is empty`, "error"); return false; }
        return true;
      });
      if (!accepted.length) return;
      if (uploadMode) uploadMode.checked = true;
      this.queue.push(...accepted);
      this.total += accepted.length;
      if (!this.running && !this.paused) this.next();
    }

    next() {
      const file = this.queue.shift();
      if (!file) {
        this.setState(`Saved ${this.done} ${plural(this.done, "file")}.`, "success");
        window.setTimeout(() => window.location.reload(), 400);
        return;
      }
      this.file = file;
      this.start();
    }

    async start() {
      this.paused = false; this.cancelled = false; this.retryIndex = 0; this.offset = 0; this.url = "";
      this.panel.hidden = false; this.progress.value = 0; this.controls.hidden = true;
      this.nameLabel.textContent = this.total > 1 ? `${this.done + 1} of ${this.total} · ${this.file.name}` : this.file.name;
      this.running = true; this.submit.disabled = true; this.fileInput.disabled = true;
      this.setState("Preparing…", "working");
      try {
        if (!window.crypto?.subtle) {
          if (this.file.size <= 100 * MiB) { await this.legacyUpload(); return this.finishOne(); }
          throw new Error("Large uploads need HTTPS in this browser");
        }
        await this.discoverChunkSize();
        this.fingerprint = await fingerprint(this.file);
        const saved = loadResumes().find((entry) => entry.fingerprint === this.fingerprint && entry.size === this.file.size);
        if (saved?.url) {
          this.url = safeSameOriginUrl(saved.url);
          if (this.url) await this.readOffset();
        }
        if (!this.url && await this.create()) return this.finishOne();
        this.startedAt = performance.now(); this.startedOffset = this.offset;
        this.showControls("uploading");
        await this.sendNext();
      } catch (error) { this.fail(error); }
    }

    finishOne() {
      this.done += 1; this.running = false;
      this.next();
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
        headers: { "Tus-Resumable": TUS_VERSION, "Upload-Length": String(this.file.size), "Upload-Metadata": encodeTusMetadata(metadata) },
      });
      if (!response.ok) {
        if ((response.status === 404 || response.status === 405) && this.file.size <= 100 * MiB) { await this.legacyUpload(); return true; }
        throw new Error(await responseMessage(response, "Could not start the upload"));
      }
      this.url = safeSameOriginUrl(response.headers.get("Location"));
      if (!this.url) throw new Error("The server returned an invalid upload address");
      saveResume({ fingerprint: this.fingerprint, url: this.url, name: this.file.name, size: this.file.size, lastModified: this.file.lastModified });
      return false;
    }

    async discoverChunkSize() {
      try {
        const response = await fetch("/api/v1/uploads", { method: "OPTIONS", credentials: "same-origin" });
        const advertised = Number(response.headers.get("X-Tus-Chunk-Size"));
        if (response.ok && Number.isSafeInteger(advertised) && advertised > 0) this.chunkSize = Math.min(advertised, CHUNK_SIZE);
      } catch {}
    }

    legacyUpload() {
      this.setState("Uploading…", "working");
      return new Promise((resolve, reject) => {
        const xhr = this.xhr = new XMLHttpRequest();
        xhr.open("POST", this.form.action);
        xhr.withCredentials = true;
        xhr.upload.addEventListener("progress", (event) => { if (event.lengthComputable) this.updateProgress(event.loaded, event.total); });
        xhr.addEventListener("load", () => (xhr.status >= 200 && xhr.status < 300 ? resolve() : reject(new Error(xhrMessage(xhr)))));
        xhr.addEventListener("error", () => reject(new Error("The upload was interrupted")));
        const body = new FormData();
        const values = new FormData(this.form);
        body.append("virtual_path", String(values.get("virtual_path") || "/"));
        body.append("tags", String(values.get("tags") || ""));
        body.append("file", this.file, this.file.name);
        xhr.send(body);
      });
    }

    async readOffset() {
      const response = await fetch(this.url, { method: "HEAD", credentials: "same-origin", headers: { "Tus-Resumable": TUS_VERSION } });
      if (response.status === 404 || response.status === 410) { removeResume(this.fingerprint); this.url = ""; this.offset = 0; return; }
      if (!response.ok) throw new Error(await responseMessage(response, "Could not resume the upload"));
      this.offset = strictOffset(response.headers.get("Upload-Offset"), this.file.size);
    }

    async sendNext() {
      if (this.cancelled || this.paused || !this.running) return;
      if (this.offset >= this.file.size) return this.pollFinalization();
      const start = this.offset;
      const end = Math.min(start + this.chunkSize, this.file.size);
      try {
        const nextOffset = await this.patchChunk(this.file.slice(start, end), start);
        if (this.paused || this.cancelled) return;
        this.offset = nextOffset; this.retryIndex = 0;
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
        xhr.upload.addEventListener("progress", (event) => { if (event.lengthComputable) this.updateProgress(offset + event.loaded, this.file.size); });
        xhr.addEventListener("load", () => {
          this.xhr = null;
          if (xhr.status >= 200 && xhr.status < 300) {
            try { resolve(strictOffset(xhr.getResponseHeader("Upload-Offset"), this.file.size)); } catch (error) { reject(error); }
          } else reject(new Error(xhrMessage(xhr)));
        });
        xhr.addEventListener("error", () => { this.xhr = null; reject(new Error("Network connection lost")); });
        xhr.addEventListener("abort", () => { this.xhr = null; const error = new Error("Upload paused"); error.name = "AbortError"; reject(error); });
        xhr.send(blob);
      });
    }

    async recover(originalError) {
      try { await this.readOffset(); } catch {}
      if (!this.url || this.retryIndex >= RETRY_DELAYS.length) return this.fail(originalError);
      const delay = RETRY_DELAYS[this.retryIndex++];
      this.setState(`Connection interrupted. Retrying in ${Math.ceil(delay / 1000)}s…`, "warning");
      await wait(delay);
      return this.sendNext();
    }

    pause() {
      if (!this.running || this.paused) return;
      this.paused = true; this.xhr?.abort();
      this.setState(`Paused at ${formatBytes(this.offset)}. Resume any time.`, "warning");
      this.showControls("paused");
    }

    async resume() {
      if (!this.file) return;
      if (!this.url) { this.running = false; return this.start(); }
      this.paused = false; this.cancelled = false; this.running = true; this.retryIndex = 0;
      this.startedAt = performance.now(); this.startedOffset = this.offset;
      this.showControls("uploading");
      try { await this.readOffset(); await this.sendNext(); } catch (error) { this.fail(error); }
    }

    async cancel() {
      const ok = await confirmAction({ title: "Cancel this upload?", text: "The part already sent is removed from the server.", ok: "Cancel upload" });
      if (!ok) return;
      this.cancelled = true; this.running = false; this.xhr?.abort(); this.queue = [];
      try {
        if (this.url) {
          const response = await fetch(this.url, { method: "DELETE", credentials: "same-origin", headers: { "Tus-Resumable": TUS_VERSION } });
          if (!response.ok && response.status !== 404 && response.status !== 410) throw new Error(await responseMessage(response, "Cancel failed"));
        }
        removeResume(this.fingerprint);
        this.setState("Upload cancelled.", "warning"); this.showControls("done");
        if (this.done) window.setTimeout(() => window.location.reload(), 600);
      } catch (error) { this.fail(error); }
    }

    async pollFinalization() {
      this.xhr = null; this.progress.value = 100; this.showControls("finalizing");
      this.setState("Verifying…", "working");
      const statusUrl = this.url.replace(/\/$/, "") + "/status";
      try {
        for (;;) {
          const response = await fetch(statusUrl, { credentials: "same-origin", headers: { Accept: "application/json" } });
          const data = await response.json().catch(() => ({}));
          if (!response.ok) throw new Error(apiMessage(data) || "Could not check the upload");
          const state = String(data.state || data.status || "").toLowerCase();
          if (["completed", "complete", "saved"].includes(state) || data.itemId || data.item_id) {
            removeResume(this.fingerprint);
            return this.finishOne();
          }
          if (["failed", "cancelled", "canceled", "expired"].includes(state)) throw new Error(data.error || data.message || `Upload ${state}`);
          await wait(800);
        }
      } catch (error) { this.fail(error); }
    }

    updateProgress(loaded, total) {
      const percent = total ? Math.min(100, (loaded / total) * 100) : 0;
      this.progress.value = percent;
      const elapsed = Math.max((performance.now() - this.startedAt) / 1000, 0.1);
      const speed = Math.max(0, loaded - this.startedOffset) / elapsed;
      const eta = speed > 0 ? (total - loaded) / speed : 0;
      this.setState(`${formatBytes(loaded)} of ${formatBytes(total)} · ${formatBytes(speed)}/s${eta > 1 ? ` · ${formatDuration(eta)} left` : ""}`, "uploading");
    }

    setState(message, state) { this.status.textContent = message; this.status.dataset.state = state; }
    showControls(state) {
      this.controls.hidden = state === "done" || state === "finalizing";
      $("[data-upload-pause]", this.form).hidden = state !== "uploading";
      $("[data-upload-resume]", this.form).hidden = state !== "paused";
      $("[data-upload-retry]", this.form).hidden = state !== "failed";
      $("[data-upload-cancel]", this.form).hidden = state === "done" || state === "finalizing";
      const idle = ["done", "failed"].includes(state);
      this.submit.disabled = !idle; this.fileInput.disabled = !idle;
    }
    fail(error) {
      this.running = false;
      this.setState(error?.message || "Upload failed", "error");
      this.showControls("failed");
      toast(error?.message || "Upload failed", "error");
    }
  }

  const uploadForm = $("[data-upload-form]");
  const uploader = uploadForm ? new ResumableUpload(uploadForm) : null;
  const dropzone = $("[data-dropzone]");
  const dropLabel = $("[data-dropzone-label]");
  const fileInput = $("input[type=file]", dropzone || document.createElement("div"));
  fileInput?.addEventListener("change", () => {
    const files = Array.from(fileInput.files || []);
    if (!dropLabel) return;
    if (!files.length) dropLabel.textContent = "Drop files here or choose them";
    else if (files.length === 1) dropLabel.textContent = `${files[0].name} · ${formatBytes(files[0].size)}`;
    else dropLabel.textContent = `${files.length} files · ${formatBytes(files.reduce((sum, file) => sum + file.size, 0))}`;
  });

  /* Drop files anywhere on the vault page. */
  const veil = $("[data-drop-veil]");
  if (uploader && veil) {
    let depth = 0;
    const hasFiles = (event) => Array.from(event.dataTransfer?.types || []).includes("Files");
    window.addEventListener("dragenter", (event) => { if (!hasFiles(event)) return; depth += 1; veil.hidden = false; });
    window.addEventListener("dragleave", () => { depth = Math.max(0, depth - 1); if (!depth) veil.hidden = true; });
    window.addEventListener("dragover", (event) => { if (hasFiles(event)) event.preventDefault(); });
    window.addEventListener("drop", (event) => {
      if (!hasFiles(event)) return;
      event.preventDefault();
      depth = 0; veil.hidden = true;
      const files = Array.from(event.dataTransfer.files || []);
      if (files.length) uploader.enqueue(files);
    });
  }

  /* Ctrl+V on the vault page. This reads only the paste the person just made;
     the page never asks for clipboard access on its own. */
  if (slot) {
    document.addEventListener("paste", (event) => {
      if (anyDialogOpen()) return;
      const data = event.clipboardData;
      if (!data) return;
      const files = Array.from(data.files || []).filter((file) => file.size > 0);
      if (files.length && uploader) {
        event.preventDefault();
        uploader.enqueue(files.map(namePastedFile));
        toast(files.length === 1 ? "Uploading pasted file" : `Uploading ${files.length} pasted files`);
        return;
      }
      if (isTyping(event.target)) return;
      const text = data.getData("text/plain");
      if (!text || !text.trim() || !captureInput) return;
      event.preventDefault();
      if (pasteMode) pasteMode.checked = true;
      captureInput.value = text;
      captureInput.focus();
      captureInput.setSelectionRange(text.length, text.length);
      toast("Pasted into the box. Press Ctrl+Enter to save.");
    });
  }
  function namePastedFile(file) {
    const generic = !file.name || /^image\.(png|jpe?g|gif|webp)$/i.test(file.name);
    if (!generic) return file;
    const extension = (file.type.split("/")[1] || "png").replace("jpeg", "jpg").replace(/[^a-z0-9]/gi, "");
    const now = new Date();
    const pad = (value) => String(value).padStart(2, "0");
    const stamp = `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())} ${pad(now.getHours())}.${pad(now.getMinutes())}.${pad(now.getSeconds())}`;
    return new File([file], `Pasted ${stamp}.${extension}`, { type: file.type, lastModified: Date.now() });
  }

  /* ---------- Storage page ---------- */
  const gauge = $("[data-gauge]");
  if (gauge) {
    const total = Number(gauge.dataset.total || 0);
    const used = Math.max(0, Number(gauge.dataset.used || 0));
    const workspace = Math.min(Math.max(0, Number(gauge.dataset.workspace || 0)), used);
    if (total > 0) {
      const share = (value) => `${Math.max(0, Math.min(100, (value / total) * 100)).toFixed(2)}%`;
      $("[data-gauge-ws]", gauge)?.style.setProperty("--w", share(workspace));
      $("[data-gauge-other]", gauge)?.style.setProperty("--w", share(used - workspace));
    }
  }
  const purgeChoices = $$("[data-purge-item]");
  if (purgeChoices.length) {
    const selectAll = $("[data-select-all-rows]");
    const summary = $("[data-purge-summary]");
    const sync = () => {
      const chosen = purgeChoices.filter((box) => box.checked);
      const bytes = chosen.reduce((sum, box) => sum + Number(box.dataset.bytes || 0), 0);
      if (summary) summary.textContent = chosen.length ? `${chosen.length} ${plural(chosen.length, "file")} · ${formatBytes(bytes)}` : "selected";
      if (selectAll) {
        selectAll.checked = chosen.length > 0 && chosen.length === purgeChoices.length;
        selectAll.indeterminate = chosen.length > 0 && chosen.length < purgeChoices.length;
      }
    };
    purgeChoices.forEach((box) => box.addEventListener("change", sync));
    selectAll?.addEventListener("change", () => { purgeChoices.forEach((box) => { box.checked = selectAll.checked; }); sync(); });
    sync();
  }
  const purgeForm = $("[data-storage-purge]");
  if (purgeForm) purgeForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const values = new FormData(purgeForm);
    if (!values.getAll("item_ids").length) return toast("Select at least one file", "error");
    const submit = $('button[type="submit"]', purgeForm); busy(submit, true);
    try {
      const response = await fetch(purgeForm.action, { method: "POST", credentials: "same-origin", body: new URLSearchParams(values) });
      if (!response.ok) throw new Error(await responseMessage(response, "Cleanup failed"));
      toast("Selected files were removed from the disk");
      window.setTimeout(() => window.location.assign("/storage"), 400);
    } catch (error) { toast(error.message, "error"); busy(submit, false); }
  });

  const storageWarning = $("[data-storage-warning]");
  const storageMeter = $("[data-storage-meter]");
  if (storageWarning || storageMeter) fetch("/api/v1/storage", { credentials: "same-origin", headers: { Accept: "application/json" } })
    .then((response) => (response.ok ? response.json() : null))
    .then((status) => {
      if (!status) return;
      const low = Boolean(status.lowStorage ?? status.low_storage);
      if (storageWarning && low) storageWarning.hidden = false;
      if (!storageMeter) return;
      const free = Number(status.freeBytes ?? status.free_bytes ?? 0);
      const total = Number(status.totalBytes ?? status.total_bytes ?? 0);
      const fill = $("[data-storage-fill]", storageMeter);
      const value = $("[data-storage-free]", storageMeter);
      const note = $("[data-storage-note]", storageMeter);
      if (total > 0) {
        const ratio = Math.max(0, Math.min(1, (total - free) / total));
        fill?.style.setProperty("--fill", `${(ratio * 100).toFixed(1)}%`);
        if (value) value.textContent = `${Math.round(ratio * 100)}%`;
        if (note) note.textContent = `${formatBytes(free)} free of ${formatBytes(total)}`;
        storageMeter.dataset.level = ratio > 0.92 ? "danger" : ratio > 0.8 ? "warn" : "ok";
      } else {
        if (value) value.textContent = formatBytes(free);
        if (note) note.textContent = "free on the server";
      }
      if (low) storageMeter.dataset.level = "danger";
      storageMeter.hidden = false;
    })
    .catch(() => {});

  /* ---------- API key page: personalised Python client ---------- */
  const keyCreated = $("[data-key-created]");
  $("[data-download-script]", keyCreated || document.createElement("div"))?.addEventListener("click", async (event) => {
    const button = event.currentTarget;
    busy(button, true);
    try {
      const response = await fetch(keyCreated.dataset.scriptSrc, { credentials: "same-origin" });
      if (!response.ok) throw new Error("Could not load the client script");
      const server = (keyCreated.dataset.server || window.location.origin).replace(/\/$/, "");
      const key = $("[data-secret]", keyCreated)?.textContent.trim() || "";
      const source = (await response.text())
        .replace('DEFAULT_SERVER = ""', `DEFAULT_SERVER = ${JSON.stringify(server)}`)
        .replace('DEFAULT_API_KEY = ""', `DEFAULT_API_KEY = ${JSON.stringify(key)}`);
      const url = URL.createObjectURL(new Blob([source], { type: "text/x-python" }));
      const link = Object.assign(document.createElement("a"), { href: url, download: "vault.py" });
      document.body.append(link); link.click(); link.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 2000);
      toast("vault.py downloaded with this key inside");
    } catch (error) { toast(error.message, "error"); }
    finally { busy(button, false); }
  });

  /* ---------- Viewer: full text, rendered documents, and media ---------- */
  const viewer = $("[data-viewer]");
  if (viewer) setupViewer(viewer);

  function setupViewer(dialog) {
    const title = $("[data-viewer-title]", dialog);
    const meta = $("[data-viewer-meta]", dialog);
    const icon = $("[data-viewer-icon]", dialog);
    const body = $("[data-viewer-body]", dialog);
    const tabs = $("[data-viewer-tabs]", dialog);
    const wrapButton = $("[data-viewer-wrap]", dialog);
    const copyButton = $("[data-viewer-copy]", dialog);
    const download = $("[data-viewer-download]", dialog);
    let controller = null;
    let current = null;
    let copyValue = "";

    const reset = () => {
      controller?.abort(); controller = null;
      $$("video, audio", body).forEach((media) => media.pause());
      body.replaceChildren();
      tabs.hidden = true; wrapButton.hidden = true; copyButton.hidden = true; copyValue = "";
    };
    const close = () => { reset(); if (dialog.open) dialog.close(); current?.querySelector("[data-open]")?.focus(); };
    $("[data-viewer-close]", dialog).addEventListener("click", close);
    dialog.addEventListener("cancel", (event) => { event.preventDefault(); close(); });
    dialog.addEventListener("click", (event) => { if (event.target === dialog) close(); });
    copyButton.addEventListener("click", () => copyText(copyValue));
    wrapButton.addEventListener("click", () => {
      const on = wrapButton.getAttribute("aria-pressed") !== "true";
      wrapButton.setAttribute("aria-pressed", String(on));
      $$(".viewer-text", body).forEach((node) => node.classList.toggle("wrap", on));
    });
    dialog.addEventListener("keydown", (event) => {
      if (isTyping(event.target)) return;
      if (event.key === "ArrowRight" || event.key === "ArrowLeft") {
        const cards = $$("[data-item-card]").filter((card) => !card.hidden);
        const index = cards.indexOf(current);
        const next = cards[index + (event.key === "ArrowRight" ? 1 : -1)];
        if (next) { event.preventDefault(); open(next); }
      }
    });

    document.addEventListener("click", (event) => {
      const trigger = event.target.closest("[data-open]");
      if (!trigger) return;
      const card = trigger.closest("[data-item-card]");
      if (card) open(card);
    });

    async function open(card) {
      reset();
      current = card;
      controller = new AbortController();
      const id = encodeURIComponent(card.dataset.id);
      const size = Number(card.dataset.size || 0);
      title.textContent = card.dataset.name || "Item";
      meta.textContent = [card.dataset.typeLabel, formatBytes(size), $("time", card)?.textContent].filter(Boolean).join(" · ");
      icon.setAttribute("href", `#${card.dataset.icon || "i-file"}`);
      download.href = `/api/v1/items/${id}/content?download=1`;
      body.replaceChildren(messageNode("Loading…"));
      if (!dialog.open) dialog.showModal();
      body.focus({ preventScroll: true });
      try {
        if ("text" in card.dataset) await openText(id, card.dataset.kind);
        else await openFile(id, card, size);
      } catch (error) {
        if (error.name !== "AbortError") body.replaceChildren(messageNode(error.message || "This item could not be shown.", true));
      }
    }

    async function openText(id, kind) {
      const response = await fetch(`/api/v1/items/${id}`, { credentials: "same-origin", headers: { Accept: "application/json" }, signal: controller.signal });
      if (!response.ok) throw new Error(await responseMessage(response, "Could not load the text"));
      const item = await response.json();
      const text = String(item.text_payload ?? item.textPayload ?? "");
      copyValue = text; copyButton.hidden = false;
      if (kind === "url") {
        const nodes = [];
        const first = text.trim().split(/\s+/)[0];
        if (/^https?:\/\//i.test(first)) {
          const link = Object.assign(document.createElement("a"), { className: "viewer-link", href: first, target: "_blank", rel: "noopener noreferrer nofollow" });
          link.innerHTML = '<svg class="i" aria-hidden="true"><use href="#i-external"></use></svg>';
          link.append(document.createTextNode(first));
          nodes.push(link);
        }
        nodes.push(textPanel(text, { wrap: true }));
        wrapButton.hidden = false; setWrap(true);
        body.replaceChildren(...nodes);
        return;
      }
      const format = kind === "html" ? "html" : kind === "code" ? "code" : looksLikeMarkdown(text) ? "markdown" : "text";
      renderDocument(text, format, false, { wrapDefault: format !== "code" && format !== "html" });
    }

    async function openFile(id, card, size) {
      const url = `/api/v1/items/${id}/preview`;
      const head = await fetch(url, { method: "HEAD", credentials: "same-origin", signal: controller.signal });
      if (!head.ok) {
        if (head.status === 415) throw new Error("This file type cannot be shown here. Use Download to open it.");
        throw new Error(await responseMessage(head, "Preview unavailable"));
      }
      const kind = classifyPreview(head.headers, card.dataset.media);
      if (kind.type === "image") {
        if (size > IMAGE_LIMIT) throw new Error(`Images over ${formatBytes(IMAGE_LIMIT)} are download only.`);
        const image = Object.assign(document.createElement("img"), { className: "media-image", alt: title.textContent, src: url });
        image.addEventListener("error", () => body.replaceChildren(messageNode("Your browser cannot display this image format. Download it to open it.", true)));
        body.replaceChildren(centered(image));
        return;
      }
      if (kind.type === "video" || kind.type === "audio") {
        const media = document.createElement(kind.type);
        media.controls = true; media.preload = "metadata"; media.src = url;
        media.addEventListener("error", () => body.replaceChildren(messageNode("Your browser cannot play this format. Download it to open it.", true)));
        body.replaceChildren(centered(media));
        return;
      }
      if (kind.type === "pdf") {
        const frame = Object.assign(document.createElement("iframe"), { title: title.textContent, src: url });
        body.replaceChildren(frame);
        return;
      }
      if (kind.type !== "text") throw new Error("This file type cannot be shown here. Use Download to open it.");
      const response = await fetch(url, { credentials: "same-origin", signal: controller.signal });
      if (!response.ok) throw new Error(await responseMessage(response, "Preview unavailable"));
      const buffer = await response.arrayBuffer();
      const text = decodeText(buffer, response.headers.get("X-Preview-Encoding"));
      const truncated = response.headers.get("X-Preview-Truncated") === "true" || size > TEXT_LIMIT;
      copyValue = truncated ? "" : text; copyButton.hidden = truncated;
      renderDocument(text, kind.format, truncated, { wrapDefault: kind.format === "text" || kind.format === "markdown", language: head.headers.get("X-Preview-Language") || "" });
    }

    function setWrap(on) {
      wrapButton.setAttribute("aria-pressed", String(on));
      $$(".viewer-text", body).forEach((node) => node.classList.toggle("wrap", on));
    }

    function renderDocument(source, format, truncated, { wrapDefault, language = "" }) {
      const notice = truncated ? noteNode(`Showing the first ${formatBytes(TEXT_LIMIT)}. Download the file to read all of it.`) : null;
      const sourcePanel = textPanel(source, { highlight: format === "code" || format === "html" || format === "markdown", language: format === "html" ? "xml" : format === "markdown" ? "markdown" : language });
      wrapButton.hidden = false;
      const canMarkdown = format === "markdown" && typeof window.marked?.parse === "function" && typeof window.DOMPurify?.sanitize === "function";
      const canHtml = format === "html" && typeof window.DOMPurify?.sanitize === "function";
      if (!canMarkdown && !canHtml) {
        if (format === "text") sourcePanel.classList.add("prose");
        body.replaceChildren(...[notice, sourcePanel].filter(Boolean));
        setWrap(wrapDefault);
        return;
      }
      const rendered = document.createElement("article");
      rendered.className = "rendered";
      rendered.innerHTML = window.DOMPurify.sanitize(canMarkdown ? window.marked.parse(source) : source, {
        USE_PROFILES: { html: true },
        FORBID_TAGS: ["style", "form", "iframe", "object", "embed", "script", "img", "picture", "source", "audio", "video", "track", "link", "input", "meta", "base"],
        FORBID_ATTR: ["style", "srcdoc"], ALLOW_UNKNOWN_PROTOCOLS: false,
      });
      $$("a", rendered).forEach((link) => { link.target = "_blank"; link.rel = "noopener noreferrer nofollow"; });
      const show = (name) => {
        const showSource = name === "source";
        sourcePanel.hidden = !showSource; rendered.hidden = showSource;
        wrapButton.hidden = !showSource;
        $$("[data-viewer-tab]", tabs).forEach((button) => button.setAttribute("aria-pressed", String(button.dataset.viewerTab === name)));
      };
      $$("[data-viewer-tab]", tabs).forEach((button) => { button.onclick = () => show(button.dataset.viewerTab); });
      tabs.hidden = false;
      body.replaceChildren(...[notice, rendered, sourcePanel].filter(Boolean));
      setWrap(wrapDefault);
      show("rendered");
    }
  }

  function textPanel(source, { highlight = false, language = "", wrap = false } = {}) {
    const pre = document.createElement("pre");
    pre.className = "viewer-text" + (wrap ? " wrap" : "");
    const code = document.createElement("code");
    code.textContent = source;
    if (language && language !== "text") code.className = `language-${language}`;
    pre.append(code);
    if (highlight && window.hljs && source.length <= HIGHLIGHT_LIMIT) {
      try {
        if (language && language !== "text" && window.hljs.getLanguage?.(language)) window.hljs.highlightElement(code);
        else {
          const result = window.hljs.highlightAuto(source);
          code.innerHTML = result.value; code.classList.add("hljs");
        }
      } catch {}
    }
    return pre;
  }
  function centered(node) { const wrap = document.createElement("div"); wrap.className = "center"; wrap.append(node); return wrap; }
  function noteNode(message) { const node = document.createElement("p"); node.className = "viewer-note"; node.textContent = message; return node; }
  function messageNode(message, error = false) { const node = document.createElement("p"); node.className = error ? "viewer-message error" : "viewer-message"; node.textContent = message; return node; }
  function looksLikeMarkdown(text) {
    if (/^```/m.test(text)) return true;
    const signals = (text.match(/^(#{1,6} |[-*+] |\d+\. |> |\|.+\|$)/gm) || []).length + (text.match(/\[[^\]]+\]\([^)]+\)|\*\*[^*]+\*\*/g) || []).length;
    return signals >= 2;
  }
  function classifyPreview(headers, media) {
    const hint = (headers.get("X-Preview-Kind") || "").toLowerCase();
    const mime = (headers.get("Content-Type") || "").split(";", 1)[0].toLowerCase();
    if (hint === "image" || mime.startsWith("image/")) return { type: "image" };
    if (hint === "video" || mime.startsWith("video/")) return { type: "video" };
    if (hint === "audio" || mime.startsWith("audio/")) return { type: "audio" };
    if (hint === "pdf" || mime === "application/pdf") return { type: "pdf" };
    if (hint === "markdown") return { type: "text", format: "markdown" };
    if (hint === "html") return { type: "text", format: "html" };
    if (hint === "code") return { type: "text", format: "code" };
    if (hint === "text" || mime.startsWith("text/")) return { type: "text", format: "text" };
    if (media === "image") return { type: "image" };
    return { type: "unsupported" };
  }

  /* ---------- Command palette ---------- */
  const palette = $("[data-palette]");
  if (palette) setupPalette(palette);

  function setupPalette(dialog) {
    const input = $("[data-palette-input]", dialog);
    const results = $("[data-palette-results]", dialog);
    const apple = /mac|iphone|ipad/i.test(navigator.platform || navigator.userAgent || "");
    $$("[data-cmd-hint]").forEach((hint) => { hint.textContent = apple ? "⌘K" : "Ctrl K"; });

    const commands = [];
    $$(".side-link").forEach((link) => {
      commands.push({ icon: $("use", link)?.getAttribute("href") || "#i-arrow", label: "Go to " + ($("span", link)?.textContent || "").trim().toLowerCase(), hint: link.getAttribute("href"), run: () => window.location.assign(link.getAttribute("href")) });
    });
    if (captureInput) {
      commands.push({ icon: "#i-plus", label: "New text", hint: "vault", run: () => { if (pasteMode) pasteMode.checked = true; captureInput.focus(); } });
      commands.push({ icon: "#i-upload", label: "Upload files", hint: "vault", run: () => { if (uploadMode) uploadMode.checked = true; fileInput?.click(); } });
    }
    if (grid) commands.push({ icon: "#i-select", label: "Select items", hint: "vault", run: () => selectToggle?.click() });
    if (emptyTrash) commands.push({ icon: "#i-trash", label: "Empty trash", hint: "trash", run: () => emptyTrash.click() });
    commands.push({ icon: "#i-download", label: "Export vault as JSON", hint: "download", run: () => window.location.assign("/api/v1/items/export?format=json") });
    commands.push({ icon: "#i-download", label: "Export vault as CSV", hint: "download", run: () => window.location.assign("/api/v1/items/export?format=csv") });
    if (collapseButton) commands.push({ icon: "#i-sidebar", label: "Collapse or expand the sidebar", hint: "[", run: () => collapseButton.click() });
    commands.push({ icon: "#i-moon", label: "Switch theme", hint: "appearance", run: () => themeToggles[0]?.click() });
    commands.push({ icon: "#i-logout", label: "Sign out", hint: "account", run: () => $("[data-signout]")?.click() });

    let rows = []; let cursor = 0; let token = 0; let timer = 0;
    const paint = () => {
      rows.forEach((row, index) => row.node.setAttribute("aria-selected", String(index === cursor)));
      rows[cursor]?.node.scrollIntoView({ block: "nearest" });
    };
    const group = (name) => { const node = document.createElement("p"); node.className = "palette-group"; node.textContent = name; results.append(node); };
    const row = (entry) => {
      const node = document.createElement("button");
      node.type = "button"; node.className = "palette-item"; node.setAttribute("role", "option");
      node.innerHTML = '<svg class="i" aria-hidden="true"><use></use></svg><strong></strong><small></small>';
      $("use", node).setAttribute("href", entry.icon);
      $("strong", node).textContent = entry.label;
      $("small", node).textContent = entry.hint || "";
      node.addEventListener("click", () => { close(); entry.run(); });
      node.addEventListener("mousemove", () => { cursor = rows.findIndex((item) => item.node === node); paint(); });
      results.append(node);
      rows.push({ node, run: entry.run });
    };
    const render = (term) => {
      results.replaceChildren(); rows = []; cursor = 0;
      const needle = term.trim().toLowerCase();
      if (needle) {
        group("Search");
        row({ icon: "#i-search", label: `Search the vault for “${term.trim()}”`, hint: "enter", run: () => window.location.assign("/?q=" + encodeURIComponent(term.trim())) });
      }
      const matches = commands.filter((command) => !needle || command.label.toLowerCase().includes(needle));
      if (matches.length) { group("Commands"); matches.slice(0, 8).forEach(row); }
      paint();
    };
    const lookup = async (term) => {
      const mine = ++token;
      try {
        const response = await fetch("/api/v1/items?limit=6&q=" + encodeURIComponent(term), { credentials: "same-origin", headers: { Accept: "application/json" } });
        if (!response.ok || mine !== token) return;
        const items = (await response.json()).items || [];
        if (mine !== token || !items.length) return;
        group("In this workspace");
        items.forEach((item) => {
          const payload = item.text_payload ?? item.textPayload ?? null;
          const name = item.original_filename ?? item.originalFilename ?? null;
          const label = (name || (payload || "").split("\n").find((line) => line.trim()) || "Untitled").trim().slice(0, 80);
          row({
            icon: payload === null ? "#i-file" : "#i-text", label, hint: payload === null ? "open" : "copy",
            run: () => {
              const card = $(`[data-item-card][data-id="${CSS.escape(item.id)}"]`);
              if (card) { $("[data-open]", card)?.click(); return; }
              if (payload === null) window.open(`/api/v1/items/${encodeURIComponent(item.id)}/content`, "_blank", "noopener");
              else copyText(payload, "Copied to your clipboard");
            },
          });
        });
        paint();
      } catch {}
    };
    const close = () => { if (dialog.open) dialog.close(); };
    const open = () => { if (dialog.open) return; dialog.showModal(); input.value = ""; render(""); input.focus(); };
    input?.addEventListener("input", () => {
      render(input.value);
      window.clearTimeout(timer);
      if (input.value.trim().length > 1) timer = window.setTimeout(() => lookup(input.value.trim()), 180);
    });
    dialog.addEventListener("keydown", (event) => {
      if (event.key === "ArrowDown") { event.preventDefault(); cursor = rows.length ? (cursor + 1) % rows.length : 0; paint(); }
      else if (event.key === "ArrowUp") { event.preventDefault(); cursor = rows.length ? (cursor - 1 + rows.length) % rows.length : 0; paint(); }
      else if (event.key === "Enter") { event.preventDefault(); const chosen = rows[cursor]; if (chosen) { close(); chosen.run(); } }
    });
    dialog.addEventListener("click", (event) => { if (event.target === dialog) close(); });
    $$("[data-palette-open]").forEach((button) => button.addEventListener("click", open));
    window.addEventListener("keydown", (event) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") { event.preventDefault(); open(); }
      else if (event.key === "/" && !isTyping(event.target) && !anyDialogOpen()) { event.preventDefault(); open(); }
    });
  }

  window.addEventListener("beforeunload", (event) => {
    if (!document.querySelector('[data-upload-status][data-state="uploading"]')) return;
    event.preventDefault(); event.returnValue = "";
  });

  /* ---------- Utilities ---------- */
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
    bytes.forEach((byte) => { binary += String.fromCharCode(byte); });
    return btoa(binary);
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
    const offset = Number(value);
    if (!Number.isSafeInteger(offset) || offset < 0 || offset > maximum) throw new Error("The server returned an invalid upload offset");
    return offset;
  }
  function safeSameOriginUrl(value) {
    if (!value) return "";
    try { const url = new URL(value, window.location.href); return url.origin === window.location.origin ? url.href : ""; } catch { return ""; }
  }
  function loadResumes() {
    try { const entries = JSON.parse(localStorage.getItem(RESUME_KEY) || "[]"); return Array.isArray(entries) ? entries.slice(-20) : []; } catch { return []; }
  }
  function saveResume(entry) {
    try { const entries = loadResumes().filter((item) => item.fingerprint !== entry.fingerprint); entries.push(entry); localStorage.setItem(RESUME_KEY, JSON.stringify(entries.slice(-20))); } catch {}
  }
  function removeResume(value) {
    try { localStorage.setItem(RESUME_KEY, JSON.stringify(loadResumes().filter((item) => item.fingerprint !== value))); } catch {}
  }
  function decodeText(buffer, encoding) {
    const bytes = new Uint8Array(buffer); let charset = encoding || "utf-8";
    if (bytes[0] === 0xff && bytes[1] === 0xfe) charset = "utf-16le";
    else if (bytes[0] === 0xfe && bytes[1] === 0xff) charset = "utf-16be";
    try { return new TextDecoder(charset, { fatal: false }).decode(bytes); } catch { return new TextDecoder("utf-8").decode(bytes); }
  }
  function plural(count, word) { return count === 1 ? word : `${word}s`; }
  function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
    const units = ["B", "KiB", "MiB", "GiB", "TiB"];
    const index = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1);
    return `${(bytes / 1024 ** index).toFixed(index ? 1 : 0)} ${units[index]}`;
  }
  function formatDuration(seconds) { if (seconds < 60) return `${Math.ceil(seconds)}s`; if (seconds < 3600) return `${Math.ceil(seconds / 60)}m`; return `${(seconds / 3600).toFixed(1)}h`; }
  function wait(milliseconds) { return new Promise((resolve) => window.setTimeout(resolve, milliseconds)); }
  function apiMessage(data) { return typeof data?.error === "string" ? data.error : data?.error?.message || data?.message || ""; }
  async function responseMessage(response, fallback) { const data = await response.json().catch(() => ({})); return apiMessage(data) || `${fallback} (${response.status})`; }
  function xhrMessage(xhr) { try { return apiMessage(JSON.parse(xhr.responseText)) || `Upload failed (${xhr.status})`; } catch { return `Upload failed (${xhr.status || "network error"})`; } }
  let toastTimer = 0;
  function toast(message, tone = "info") {
    const element = document.getElementById("toast");
    if (!element) return;
    element.textContent = message;
    element.dataset.tone = tone;
    element.hidden = false;
    window.clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => { element.hidden = true; }, tone === "error" ? 6000 : 3500);
  }
})();
