# Clipboard Vault

Clipboard Vault is a private, searchable clipboard history for text, code, links,
HTML, and images. It reads the browser clipboard only while the page is focused,
hashes clips before upload, skips duplicates, pauses on focus loss, and queues
offline writes for replay after reconnection.

## Product surfaces

- Live clipboard capture with clear polling, paused, and permission states
- Search, type filters, pinned clips, tags, bulk selection, and keyboard shortcuts
- Sandboxed HTML previews, image resizing, and one-click copy-back
- Overview metrics, CSS-native charts, activity history, export, and retention policy
- Soft-delete trash with restore and permanent deletion
- JSON, JSONL, and CSV exports
- Installable service-worker shell with stale-while-revalidate item reads

## Privacy and identity

The deployed application is protected by dispatch-owned Sign in with ChatGPT.
Server routes verify the forwarded authenticated user before reading or changing
vault data. Local development uses an isolated preview identity. Security headers
deny framing and disable unnecessary device capabilities.

## Storage

Structured history, settings, and activity records use Cloudflare D1 through the
logical `DB` binding in `.openai/hosting.json`. Runtime initialization is safe and
idempotent, and the checked-in Drizzle migration is the deployment source of
truth. The content hash has a unique index for race-safe de-duplication.

## Local development

Requires Node.js 22.13 or newer.

```bash
npm install
npm run dev
```

Generate a migration after schema changes:

```bash
npm run db:generate
```

Validate the production bundle and server-rendered product shell:

```bash
npm test
```
