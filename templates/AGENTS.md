# Server-rendered frontend guide

This directory contains Askama HTML templates rendered by `src/pages.rs`. It is the markup half of the frontend; also read `../static/AGENTS.md` for the shared design system and browser behavior.

## Template structure

- `base.html` owns metadata, security-compatible asset loading, the shared header, theme control, main shell, and toast region.
- `dashboard.html` owns capture, upload, filtering, item actions, and the file preview dialog.
- `login.html`, `setup.html`, `pending.html`, `join.html`, and `reset_password.html` cover the identity lifecycle.
- `keys.html`, `storage.html`, and `admin.html` cover workspace access, cleanup, and global administration.
- `one_time_secret.html` displays sensitive values exactly once.

## Backend/template contract

- Template structs and fields are declared in `src/pages.rs`. Any new interpolation must be supplied there with the correct type.
- Form `action`, method, input names, and enum values must match the Rust page handlers.
- Conditional UI is not authorization. Rust must enforce the same permission regardless of whether a button or section is hidden.
- Escape user content by default. Keep HTML previews sandboxed and source views inert.
- Never render complete secrets into reusable pages, logs, `data-*` attributes, or URLs. One-time secret pages are the narrow exception.
- Treat form actions, field names, enum values, and `data-*` hooks as versioned contracts for the deployed VPS. Flag changes that require coordinated backend/browser deployment or make cached/older pages incompatible.

## Browser integration

- `static/app.js` discovers elements through stable `data-*` attributes such as `data-auth-form`, `data-signout`, `data-upload-form`, `data-item-card`, and `data-preview-*`.
- Treat these attributes, input names, and dialog structure as an interface. Update JavaScript in the same change if the markup contract moves.
- Prefer normal links and forms for navigation and mutations. Use JavaScript to enhance uploads, previews, copying, filtering, and auth requests.
- Keep every form usable with clear labels, validation attributes, error/status regions, and keyboard focus behavior.

## Design rules

- Reuse classes and tokens from `static/app.css`; do not introduce page-local inline styles or a separate theme system.
- Clipboard Vault has one visual language with explicit dark and light modes.
- Preserve the restrained information hierarchy: one primary action per surface, compact technical metadata, and minimal decorative chrome.
- Use semantic HTML first. Preserve landmarks, heading order, table headers, accessible names, live regions, dialog labels, and visible focus states.
- Verify narrow layouts when adding controls to the shared header, tables, cards, or two-column panels.

## Validation

- Confirm every referenced field exists in its Askama template struct.
- Confirm every form route and field name matches `src/lib.rs` and `src/pages.rs`.
- Confirm JavaScript selectors still match the rendered `data-*` hooks.
- Run Rust formatting/tests because Askama templates are compiled with the Rust application.
- Run `python -m pytest -q tests` for browser/security contracts.
