/* Runs before first paint so the correct theme is applied without a flash.
   Kept as its own file because the page CSP forbids inline scripts.
   AMOLED dark is the default; light applies only when the person chose it.
   The collapsed sidebar is restored here too, so the layout never jumps. */
(() => {
  const root = document.documentElement;
  try {
    const stored = localStorage.getItem("clipboard-vault.theme");
    root.dataset.theme = stored === "light" ? "light" : "dark";
    if (localStorage.getItem("clipboard-vault.sidebar") === "collapsed") root.dataset.sidebar = "collapsed";
  } catch (_) {
    root.dataset.theme = "dark";
  }
})();
