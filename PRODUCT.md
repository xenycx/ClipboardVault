# Clipboard Vault — product context

> Written from the owner's redesign brief (November 2026) without an interview, at the
> owner's request ("don't ask me questions"). Lines marked *assumption* were inferred.

## What it is

A self-hosted vault for things a person explicitly hands it: pasted text and code, links,
screenshots and other images, and files up to 10 GiB. It runs on the owner's VPS and is
also driven from scripts through workspace API keys.

## Who uses it

- The owner, daily, on a desktop browser: pasting snippets, dropping screenshots,
  grabbing files back out, and handing the vault to small Python scripts.
- A handful of invited people sharing a workspace. *Assumption:* fewer than 20 accounts.
- The owner as server administrator: approving or creating accounts, closing
  registration, setting upload limits.

## Primary jobs

1. Put something in fast: Ctrl+V anywhere (text or image), drag a file, or run a script.
2. Find it and read all of it: full text, rendered Markdown/HTML, images, audio, video, PDF.
3. Clean up in bulk: select many, move to trash, empty the trash.
4. Administer the server without touching the database.

## Constraints

- Production install with live data; every change must upgrade in place.
- Strict CSP (`script-src 'self'`), no inline script, no CDN at runtime.
- The app never reads the clipboard on its own; only an explicit user paste or copy.

## Tone

Plain, exact, quiet. Words name actions. Errors state the problem and the fix.

## Visual brief (owner's words)

- "UI is a little bit dated and looks like AI slop."
- "Don't guess the sizes … everything should be even."
- "Make the theme not blue, just AMOLED with good tints."
