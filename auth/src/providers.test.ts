import assert from "node:assert/strict";
import test from "node:test";
import {
  buildProviderRegistry,
  selectVerifiedDiscordEmail,
  selectVerifiedGitHubEmail,
} from "./providers.js";

test("disables providers without credentials", () => {
  const registry = buildProviderRegistry({});
  assert.deepEqual(registry.socialProviders, {});
  assert.deepEqual(registry.publicProviders, []);
});

test("rejects every incomplete credential pair with the missing variable", () => {
  const pairs = [
    ["GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"],
    ["GITHUB_CLIENT_ID", "GITHUB_CLIENT_SECRET"],
    ["DISCORD_CLIENT_ID", "DISCORD_CLIENT_SECRET"],
  ] as const;

  for (const [clientIdVariable, clientSecretVariable] of pairs) {
    assert.throws(
      () => buildProviderRegistry({ [clientIdVariable]: "client" }),
      new RegExp(clientSecretVariable),
    );
    assert.throws(
      () => buildProviderRegistry({ [clientSecretVariable]: "secret" }),
      new RegExp(clientIdVariable),
    );
  }
});

test("keeps stable public ordering and requests only identity scopes", () => {
  const registry = buildProviderRegistry({
    GOOGLE_CLIENT_ID: "google-client",
    GOOGLE_CLIENT_SECRET: "google-secret",
    GITHUB_CLIENT_ID: "github-client",
    GITHUB_CLIENT_SECRET: "github-secret",
    DISCORD_CLIENT_ID: "discord-client",
    DISCORD_CLIENT_SECRET: "discord-secret",
  });

  assert.deepEqual(registry.publicProviders, [
    { id: "google", label: "Google" },
    { id: "github", label: "GitHub" },
    { id: "discord", label: "Discord" },
  ]);
  assert.deepEqual(registry.socialProviders.google?.scope, ["openid", "email", "profile"]);
  assert.deepEqual(registry.socialProviders.github?.scope, ["read:user", "user:email"]);
  assert.deepEqual(registry.socialProviders.discord?.scope, ["identify", "email"]);
  assert.equal(registry.socialProviders.google?.disableDefaultScope, true);
});

test("never exposes credentials in public metadata", () => {
  const registry = buildProviderRegistry({
    GITHUB_CLIENT_ID: "public-client-id",
    GITHUB_CLIENT_SECRET: "do-not-expose",
  });
  const metadata = JSON.stringify(registry.publicProviders);

  assert.equal(metadata.includes("public-client-id"), false);
  assert.equal(metadata.includes("do-not-expose"), false);
  assert.deepEqual(registry.publicProviders, [{ id: "github", label: "GitHub" }]);
});

test("accepts only provider-verified GitHub and Discord emails", () => {
  assert.equal(selectVerifiedGitHubEmail([
    { email: "unverified@example.test", primary: true, verified: false },
    { email: "verified@example.test", primary: false, verified: true },
  ]), "verified@example.test");
  assert.equal(selectVerifiedGitHubEmail([{ email: "no@example.test", verified: false }]), null);
  assert.equal(selectVerifiedGitHubEmail([]), null);

  assert.equal(selectVerifiedDiscordEmail({ email: "verified@example.test", verified: true }), "verified@example.test");
  assert.equal(selectVerifiedDiscordEmail({ email: "no@example.test", verified: false }), null);
  assert.equal(selectVerifiedDiscordEmail({ email: null, verified: true }), null);
});
