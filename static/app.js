(() => {
  "use strict";
  const errorBox = document.querySelector("[data-auth-error]");
  const showError = (message) => { if (errorBox) errorBox.textContent = message || "Something went wrong."; };
  const requestedReturn = new URLSearchParams(window.location.search).get("returnTo") || "/pending";
  const returnTo = requestedReturn.startsWith("/") && !requestedReturn.startsWith("//") ? requestedReturn : "/pending";
  const authRequest = async (path, body) => {
    const response = await fetch(path, {
      method: "POST", credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(data.message || (data.error && data.error.message) || "Request failed");
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
  const google = document.querySelector("[data-google-login]");
  if (google) google.addEventListener("click", async () => {
    try {
      const data = await authRequest("/api/auth/sign-in/social", { provider: "google", callbackURL: returnTo });
      if (data.url) window.location.assign(data.url);
    } catch (error) { showError(error.message); }
  });
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
      const response = await fetch("/api/v1/items/" + button.dataset.deleteId, { method: "DELETE", credentials: "same-origin" });
      if (response.ok) button.closest("[data-item-card]")?.remove(); else toast("Delete failed");
    });
  });
  const filter = document.querySelector("[data-item-filter]");
  if (filter) filter.addEventListener("input", () => {
    const term = filter.value.trim().toLowerCase();
    document.querySelectorAll("[data-item-card]").forEach((card) => {
      card.hidden = Boolean(term && !card.textContent.toLowerCase().includes(term));
    });
  });
  const uploadForm = document.querySelector("[data-upload-form]");
  if (uploadForm) uploadForm.addEventListener("submit", (event) => {
    event.preventDefault();
    const progress = uploadForm.querySelector("[data-upload-progress]");
    const xhr = new XMLHttpRequest();
    xhr.open("POST", uploadForm.action);
    xhr.upload.addEventListener("progress", (progressEvent) => {
      progress.hidden = false;
      if (progressEvent.lengthComputable) progress.value = (progressEvent.loaded / progressEvent.total) * 100;
    });
    xhr.addEventListener("load", () => {
      if (xhr.status >= 200 && xhr.status < 300) window.location.reload();
      else {
        let message = "Upload failed";
        try { message = JSON.parse(xhr.responseText).error?.message || message; } catch {}
        toast(message); progress.hidden = true;
      }
    });
    xhr.addEventListener("error", () => toast("Upload interrupted"));
    xhr.send(new FormData(uploadForm));
  });
  const fileInput = document.querySelector("[data-dropzone] input[type=file]");
  if (fileInput) fileInput.addEventListener("change", () => {
    const name = fileInput.files && fileInput.files[0] && fileInput.files[0].name;
    if (name) document.querySelector("[data-dropzone] strong").textContent = name;
  });
  function toast(message) {
    const element = document.getElementById("toast");
    if (!element) return;
    element.textContent = message; element.hidden = false;
    window.setTimeout(() => { element.hidden = true; }, 3000);
  }
})();
