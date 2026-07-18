import assert from "node:assert/strict";
import { access, readFile } from "node:fs/promises";
import test from "node:test";

test("defines the protected Clipboard Vault product shell", async () => {
  const [page, layout, client] = await Promise.all([
    readFile(new URL("../app/page.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/layout.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/VaultApp.tsx", import.meta.url), "utf8"),
  ]);
  assert.match(page, /requireChatGPTUser\("\/"\)/);
  assert.match(layout, /Clipboard Vault/);
  assert.match(layout, /\/og\.png/);
  assert.match(client, /Read clipboard/);
  assert.match(client, /PRIVATE CLIPBOARD MEMORY/);
  assert.match(client, /role="region"/);
  assert.doesNotMatch(`${page}\n${layout}\n${client}`, /codex-preview|Your site is taking shape|react-loading-skeleton/i);
});

test("defines the protected overview route and admin surfaces", async () => {
  const [admin, client] = await Promise.all([
    readFile(new URL("../app/admin/page.tsx", import.meta.url), "utf8"),
    readFile(new URL("../app/VaultApp.tsx", import.meta.url), "utf8"),
  ]);
  assert.match(admin, /requireChatGPTUser\("\/admin"\)/);
  assert.match(admin, /initialSection="dashboard"/);
  assert.match(client, /VAULT OVERVIEW/);
  assert.match(client, /Your clipboard at a glance/);
  assert.match(client, /Recently deleted/);
  assert.match(client, /Automatic housekeeping/);
});

test("ships persistence, offline replay, and privacy safeguards", async () => {
  const [client, hosting, schema, serviceWorker, migration, packageJson] =
    await Promise.all([
      readFile(new URL("../app/VaultApp.tsx", import.meta.url), "utf8"),
      readFile(new URL("../.openai/hosting.json", import.meta.url), "utf8"),
      readFile(new URL("../db/schema.ts", import.meta.url), "utf8"),
      readFile(new URL("../public/sw.js", import.meta.url), "utf8"),
      readFile(new URL("../drizzle/0000_handy_vivisector.sql", import.meta.url), "utf8"),
      readFile(new URL("../package.json", import.meta.url), "utf8"),
    ]);

  assert.match(hosting, /"d1": "DB"/);
  assert.match(schema, /uniqueIndex\("idx_items_hash"\)/);
  assert.match(migration, /CREATE TABLE `clipboard_items`/);
  assert.match(serviceWorker, /stale|cached \|\| network/i);
  assert.match(client, /document\.hidden|document\.hasFocus/);
  assert.match(client, /indexedDB\.open\("clipboardQueue"/);
  assert.match(client, /crypto\.subtle\.digest\("SHA-256"/);
  assert.match(client, /<iframe sandbox=""/);
  assert.doesNotMatch(packageJson, /react-loading-skeleton/);
  await assert.rejects(access(new URL("../app/_sites-preview", import.meta.url)));
});
