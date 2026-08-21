/* Runs before first paint so the correct theme is applied without a flash.
   Kept as its own file because the page CSP forbids inline scripts. */
(() => {
  const root = document.documentElement;
  try {
    const stored = localStorage.getItem("clipboard-vault.theme");
    if (stored === "light" || stored === "dark") {
      root.dataset.theme = stored;
      return;
    }
    const light = window.matchMedia && window.matchMedia("(prefers-color-scheme: light)").matches;
    root.dataset.theme = light ? "light" : "dark";
  } catch (_) {
    root.dataset.theme = "dark";
  }
})();
