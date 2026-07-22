# Browser frontend guide

This directory is the asset half of the server-rendered frontend. There is no bundler or frontend framework. Axum serves these files directly under `/static`; markup lives in `../templates/`.

## Asset ownership

- `app.css`: the complete responsive design system, dark/light theme tokens, shared components, page layouts, dialogs, tables, and accessibility states.
- `app.js`: progressive enhancement for authentication, sign-out, copy/delete/filter actions, resumable uploads, storage purge, and file previews.
- `vendor/`: pinned, locally served third-party preview libraries and styles. Do not edit minified vendor files manually.

## DOM contract

- JavaScript binds to `data-*` hooks in the Askama templates. Query for behavior hooks, not presentation classes.
- Keep behavior initialization optional and page-safe: every selector may be absent on another page.
- When adding a hook, update the corresponding template and preserve accessible labels/status elements.
- Avoid rendering application UI entirely in JavaScript. Rust and Askama own primary content and navigation.
- The VPS serves these assets alongside server-rendered HTML. Keep templates and browser hooks deployable together, and identify any change that would make cached/older HTML or clients incompatible with the new assets or API.

## Security and privacy rules

- Never call `navigator.clipboard.read`, `readText`, clipboard events, polling, or background clipboard access. Only explicit user-triggered `navigator.clipboard.writeText` is permitted.
- Keep the application compatible with the self-only CSP in `src/lib.rs`: no CDN scripts, inline event handlers, `eval`, remote fonts, or unexpected network origins.
- Sanitize Markdown/HTML through the vendored libraries before inserting rendered content. Preserve source mode and safe link attributes.
- Do not place session tokens, API keys, reset tokens, file contents, or private metadata in local storage. Theme preference and resumable-upload metadata are the intended local-storage uses.
- Use same-origin URLs for preview, download, upload, and cleanup requests.

## Upload rules

- The browser must not read the whole file into memory. Upload large files as bounded chunks.
- Treat the server's Tus offset as authoritative on create, resume, retry, and conflict recovery.
- Preserve pause, resume, retry, cancel, expiry, progress, unload warning, and finalization polling behavior.
- Keep chunk sizes and server-discovery behavior aligned with the Rust upload implementation and Nginx limits.
- Do not remove the multipart path; it remains the compatibility path for smaller files.

## Preview rules

- Load a preview only after an explicit click.
- Preserve preview-size limits, truncation messaging, text encoding handling, syntax-highlight limits, safe HTML/Markdown rendering, image errors, PDF Range behavior, and the universal Download action.
- Unknown or unsafe formats should produce a clear message rather than attempted execution.

## CSS and theming rules

- Use the existing custom properties. Add a semantic token before repeating hard-coded color values across components.
- Dark and light are two modes of the same design, not independent themes.
- Keep contrast, visible focus, reduced-motion behavior, touch targets, overflow handling, and mobile navigation intact.
- Avoid inline style attributes and CSS that depends on JavaScript for basic readability.

## Validation

- Run `python -m pytest -q tests`; it enforces the no-clipboard-read and upload contracts.
- Compile the Rust application to catch template integration issues.
- Exercise affected flows at desktop and narrow widths when markup or CSS changes.
- For upload or preview changes, rely on the full GitHub Actions integration job before merging.
