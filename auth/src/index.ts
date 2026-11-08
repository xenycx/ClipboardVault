import { timingSafeEqual } from "node:crypto";
import { serve } from "@hono/node-server";
import { Hono } from "hono";
import { auth, pendingResetTokens, pool, publicBaseURL, serverSettings } from "./auth.js";
import { providerRegistry } from "./providers.js";

type SessionShape = {
  user: {
    id: string; name: string; email: string; image?: string | null;
    role?: string | null; approvalStatus?: string | null;
  };
};

const app = new Hono();
const bridgeSecret = process.env.AUTH_BRIDGE_SECRET || "";
const bootstrapToken = process.env.BOOTSTRAP_TOKEN || "";
if (bridgeSecret.length < 32) throw new Error("AUTH_BRIDGE_SECRET must contain at least 32 characters");
if (bootstrapToken.length < 32) throw new Error("BOOTSTRAP_TOKEN must contain at least 32 characters");

const data = <T>(value: T) => ({ data: value });
const safeEqual = (left: string, right: string) => {
  const a = Buffer.from(left); const b = Buffer.from(right);
  return a.length === b.length && timingSafeEqual(a, b);
};
const requireBridge = (header: string | undefined) => safeEqual(header || "", bridgeSecret);
const requireSameOrigin = (origin: string | undefined) => !origin || origin.replace(/\/$/, "") === publicBaseURL;
async function getSession(headers: Headers): Promise<SessionShape | null> {
  return (await auth.api.getSession({ headers })) as SessionShape | null;
}
async function requireAdmin(requesterId: string) {
  const result = await pool.query('SELECT role, "approvalStatus" FROM "user" WHERE id = $1', [requesterId]);
  const row = result.rows[0];
  if (!row || row.approvalStatus !== "approved" || !String(row.role || "").split(",").includes("admin")) {
    throw new Error("FORBIDDEN");
  }
}
async function membership(userId: string, organizationId: string) {
  const result = await pool.query('SELECT role FROM member WHERE "userId" = $1 AND "organizationId" = $2', [userId, organizationId]);
  return result.rows[0]?.role as string | undefined;
}
async function ensurePersonalWorkspace(userId: string, name: string) {
  const existing = await pool.query('SELECT 1 FROM member WHERE "userId" = $1 LIMIT 1', [userId]);
  if (existing.rowCount) return;
  const stem = name.toLowerCase().normalize("NFKD").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "workspace";
  await auth.api.createOrganization({
    body: { name: name + "'s Vault", slug: stem + "-" + userId.slice(0, 8).toLowerCase(), userId },
  });
}
const isAdminRole = (role: unknown) => String(role || "").split(",").map((part) => part.trim()).includes("admin");
const refuse = (message: string) => ({ message });
async function approveUser(userId: string, approvedBy: string | null) {
  const result = await pool.query(
    'UPDATE "user" SET "approvalStatus" = \'approved\', "approvedAt" = now(), "approvedBy" = $2, "updatedAt" = now() WHERE id = $1 RETURNING id, name',
    [userId, approvedBy],
  );
  if (result.rowCount) await ensurePersonalWorkspace(result.rows[0].id, result.rows[0].name);
  return Boolean(result.rowCount);
}
function keyPermissions(value: unknown): string[] {
  let permissions = value;
  if (typeof permissions === "string") {
    try { permissions = JSON.parse(permissions); } catch { return []; }
  }
  if (!permissions || typeof permissions !== "object") return [];
  return Object.entries(permissions as Record<string, unknown>).flatMap(([resource, actions]) =>
    Array.isArray(actions) ? actions.map((action) => resource + ":" + String(action)) : [],
  );
}
function keyMetadata(value: unknown): Record<string, unknown> {
  if (typeof value === "string") {
    try { return JSON.parse(value) as Record<string, unknown>; } catch { return {}; }
  }
  return value && typeof value === "object" ? value as Record<string, unknown> : {};
}

app.get("/health", async (c) => {
  await pool.query("SELECT 1"); return c.json(data({ status: "ready" }));
});

app.get("/api/auth/vault/providers", (c) => {
  c.header("Cache-Control", "no-store");
  return c.json(data({ providers: providerRegistry.publicProviders }));
});

app.post("/api/auth/vault/bootstrap", async (c) => {
  if (!requireSameOrigin(c.req.header("origin"))) return c.json({ message: "Invalid origin" }, 403);
  const session = await getSession(c.req.raw.headers);
  if (!session) return c.json({ message: "Sign in first" }, 401);
  const body = await c.req.json<{ token?: string }>();
  if (!safeEqual(body.token || "", bootstrapToken)) return c.json({ message: "Invalid setup token" }, 403);
  const admins = await pool.query("SELECT count(*)::int AS count FROM \"user\" WHERE string_to_array(coalesce(role,''), ',') @> ARRAY['admin']");
  if (admins.rows[0]?.count > 0) return c.json({ message: "Setup is already complete" }, 409);
  await pool.query("UPDATE \"user\" SET role = 'admin', \"approvalStatus\" = 'approved', \"approvedAt\" = now(), \"updatedAt\" = now() WHERE id = $1", [session.user.id]);
  await ensurePersonalWorkspace(session.user.id, session.user.name);
  return c.json({ ok: true });
});

app.get("/internal/session", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const session = await getSession(c.req.raw.headers);
  if (!session) return c.json({ message: "Unauthorized" }, 401);
  const userResult = await pool.query('SELECT id, name, email, image, role, "approvalStatus", banned FROM "user" WHERE id = $1', [session.user.id]);
  const user = userResult.rows[0];
  if (!user || user.banned) return c.json({ message: "Unauthorized" }, 401);
  delete user.banned;
  if (user.approvalStatus === "pending" && !(await serverSettings()).requireApproval) {
    // Approval was switched off by an administrator: let waiting accounts straight in.
    await approveUser(user.id, null);
    user.approvalStatus = "approved";
  }
  const membershipsResult = await pool.query(
    'SELECT m."organizationId" AS "organizationId", m.role, o.name AS "organizationName", o.slug AS "organizationSlug" FROM member m JOIN organization o ON o.id = m."organizationId" WHERE m."userId" = $1 ORDER BY m."createdAt" ASC',
    [user.id],
  );
  const requested = c.req.header("x-workspace-id");
  const active = (requested ? membershipsResult.rows.find((row) => row.organizationId === requested) : undefined)
    ?? membershipsResult.rows[0];
  return c.json(data({
    user, activeOrganizationId: active?.organizationId ?? null, activeRole: active?.role ?? null,
    memberships: membershipsResult.rows,
  }));
});

app.post("/internal/verify-key", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ key?: string; permission?: string }>();
  if (!body.key || !body.permission) return c.json({ message: "Invalid key request" }, 400);
  const parts = body.permission.split(":"); const resource = parts[0]; const action = parts[1];
  if (!resource || !action) return c.json({ message: "Invalid permission" }, 400);
  const result = await auth.api.verifyApiKey({ body: { key: body.key, permissions: { [resource]: [action] } } });
  if (!result.valid || !result.key) return c.json({ message: "Invalid API key" }, 401);
  const meta = keyMetadata(result.key.metadata);
  return c.json(data({
    id: result.key.id, organizationId: result.key.referenceId,
    createdByUserId: typeof meta.createdByUserId === "string" ? meta.createdByUserId : null,
    permissions: keyPermissions(result.key.permissions),
  }));
});

app.get("/internal/api-keys", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const organizationId = c.req.query("organizationId") || ""; const requesterId = c.req.query("requesterId") || "";
  const role = await membership(requesterId, organizationId);
  if (!role) return c.json({ message: "Forbidden" }, 403);
  const result = await pool.query('SELECT id, name, start, metadata, permissions, "expiresAt", "createdAt" FROM apikey WHERE "referenceId" = $1 ORDER BY "createdAt" DESC', [organizationId]);
  const keys = result.rows.map((row) => {
    const meta = keyMetadata(row.metadata);
    return {
      id: row.id, name: row.name, start: row.start || "cv_live_",
      createdByUserId: meta.createdByUserId || null, permissions: keyPermissions(row.permissions),
      expiresAt: row.expiresAt, createdAt: row.createdAt,
    };
  }).filter((key) => role === "owner" || role === "admin" || key.createdByUserId === requesterId);
  return c.json(data({ keys }));
});

app.post("/internal/api-keys", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ organizationId: string; requesterId: string; name: string; expiresInDays: number; permissions: string[] }>();
  const role = await membership(body.requesterId, body.organizationId);
  if (!role) return c.json({ message: "Forbidden" }, 403);
  const permissions: Record<string, string[]> = {};
  for (const permission of body.permissions || []) {
    const parts = permission.split(":"); const resource = parts[0]; const action = parts[1];
    if (resource === "items" && action && ["read", "write", "delete"].includes(action)) {
      (permissions[resource] ||= []).push(action);
    }
  }
  if (!permissions.items?.includes("write")) permissions.items = [...(permissions.items || []), "write"];
  const result = await auth.api.createApiKey({
    body: {
      name: String(body.name || "API key").slice(0, 80), organizationId: body.organizationId,
      userId: body.requesterId, expiresIn: Math.max(1, Math.min(3650, body.expiresInDays || 90)) * 86400,
      prefix: "cv_live_", permissions, metadata: { createdByUserId: body.requesterId },
      rateLimitEnabled: true, rateLimitTimeWindow: 60000, rateLimitMax: 120,
    },
  });
  return c.json(data({ secret: result.key }));
});

app.delete("/internal/api-keys/:id", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const organizationId = c.req.query("organizationId") || ""; const requesterId = c.req.query("requesterId") || "";
  const role = await membership(requesterId, organizationId);
  if (!role) return c.json({ message: "Forbidden" }, 403);
  const found = await pool.query('SELECT metadata FROM apikey WHERE id = $1 AND "referenceId" = $2', [c.req.param("id"), organizationId]);
  if (!found.rowCount) return c.json({ message: "Not found" }, 404);
  const creator = keyMetadata(found.rows[0].metadata).createdByUserId;
  if (!["owner", "admin"].includes(role) && creator !== requesterId) return c.json({ message: "Forbidden" }, 403);
  await pool.query('DELETE FROM apikey WHERE id = $1 AND "referenceId" = $2', [c.req.param("id"), organizationId]);
  return c.json(data({ ok: true }));
});

app.get("/internal/admin/users", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const requesterId = c.req.query("requesterId") || "";
  try { await requireAdmin(requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  const result = await pool.query('SELECT id, name, email, coalesce(role,\'user\') AS role, coalesce("approvalStatus",\'pending\') AS "approvalStatus", banned, "createdAt" FROM "user" ORDER BY "createdAt" DESC LIMIT 1000');
  return c.json(data({ users: result.rows }));
});

for (const action of ["approve", "reject"] as const) {
  app.post("/internal/admin/" + action, async (c) => {
    if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
    const body = await c.req.json<{ requesterId: string; userId: string }>();
    try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
    const status = action === "approve" ? "approved" : "rejected";
    if (status === "rejected") {
      const targetRole = await pool.query('SELECT role FROM "user" WHERE id = $1', [body.userId]);
      if (String(targetRole.rows[0]?.role || "").split(",").includes("admin")) {
        return c.json({ message: "Global administrators cannot be rejected" }, 403);
      }
    }
    const result = await pool.query(
      'UPDATE "user" SET "approvalStatus" = $1, "approvedAt" = CASE WHEN $1 = \'approved\' THEN now() ELSE NULL END, "approvedBy" = $2, "updatedAt" = now() WHERE id = $3 RETURNING id, name',
      [status, body.requesterId, body.userId],
    );
    if (!result.rowCount) return c.json({ message: "Not found" }, 404);
    if (status === "approved") await ensurePersonalWorkspace(result.rows[0].id, result.rows[0].name);
    return c.json(data({ ok: true }));
  });
}

app.post("/internal/admin/create-user", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ requesterId: string; name?: string; email?: string; password?: string; role?: string }>();
  try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  const name = String(body.name || "").trim().slice(0, 80);
  const email = String(body.email || "").trim().toLowerCase();
  const password = String(body.password || "");
  if (!name) return c.json(refuse("Enter a name for the account."), 400);
  if (!/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(email)) return c.json(refuse("Enter a valid email address."), 400);
  if (password.length < 8 || password.length > 128) return c.json(refuse("Passwords must be 8 to 128 characters."), 400);
  const existing = await pool.query('SELECT 1 FROM "user" WHERE lower(email) = $1', [email]);
  if (existing.rowCount) return c.json(refuse("An account with that email already exists."), 400);
  const role = body.role === "admin" ? "admin" : "user";
  const created = await auth.api.createUser({ body: { email, password, name, role } });
  await approveUser(created.user.id, body.requesterId);
  return c.json(data({ id: created.user.id }));
});

app.post("/internal/admin/set-role", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ requesterId: string; userId: string; role: string }>();
  try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  const makeAdmin = body.role === "admin";
  const target = await pool.query('SELECT role, banned FROM "user" WHERE id = $1', [body.userId]);
  if (!target.rowCount) return c.json({ message: "Not found" }, 404);
  if (!makeAdmin && body.userId === body.requesterId) return c.json(refuse("You cannot remove your own administrator role."), 400);
  if (makeAdmin && target.rows[0].banned) return c.json(refuse("Enable the account before making it an administrator."), 400);
  await pool.query('UPDATE "user" SET role = $1, "updatedAt" = now() WHERE id = $2', [makeAdmin ? "admin" : "user", body.userId]);
  if (makeAdmin) await approveUser(body.userId, body.requesterId);
  return c.json(data({ ok: true }));
});

app.post("/internal/admin/ban", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ requesterId: string; userId: string; banned: boolean }>();
  try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  const target = await pool.query('SELECT role FROM "user" WHERE id = $1', [body.userId]);
  if (!target.rowCount) return c.json({ message: "Not found" }, 404);
  const banned = body.banned === true;
  if (banned && body.userId === body.requesterId) return c.json(refuse("You cannot disable your own account."), 400);
  if (banned && isAdminRole(target.rows[0].role)) return c.json(refuse("Remove the administrator role before disabling this account."), 400);
  await pool.query(
    'UPDATE "user" SET banned = $1, "banReason" = CASE WHEN $1 THEN \'Disabled by an administrator\' ELSE NULL END, "banExpires" = NULL, "updatedAt" = now() WHERE id = $2',
    [banned, body.userId],
  );
  if (banned) await pool.query('DELETE FROM session WHERE "userId" = $1', [body.userId]);
  return c.json(data({ ok: true }));
});

app.post("/internal/admin/revoke-sessions", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ requesterId: string; userId: string }>();
  try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  const result = await pool.query('DELETE FROM session WHERE "userId" = $1', [body.userId]);
  return c.json(data({ ok: true, revoked: result.rowCount ?? 0 }));
});

app.post("/internal/admin/delete-user", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ requesterId: string; userId: string; confirmation: string }>();
  try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  if (body.userId === body.requesterId) return c.json(refuse("You cannot delete your own account."), 400);
  const target = await pool.query('SELECT email, role FROM "user" WHERE id = $1', [body.userId]);
  if (!target.rowCount) return c.json({ message: "Not found" }, 404);
  if (isAdminRole(target.rows[0].role)) return c.json(refuse("Remove the administrator role before deleting this account."), 400);
  if (String(body.confirmation || "").trim().toLowerCase() !== String(target.rows[0].email).toLowerCase()) {
    return c.json(refuse("Type the account's email address exactly to confirm deletion."), 400);
  }
  const owned = await pool.query(
    `SELECT o.id, o.name, (SELECT count(*)::int FROM member other WHERE other."organizationId" = o.id AND other."userId" <> $1) AS others
     FROM member m JOIN organization o ON o.id = m."organizationId" WHERE m."userId" = $1 AND m.role = 'owner'`,
    [body.userId],
  );
  const shared = owned.rows.find((row) => row.others > 0);
  if (shared) return c.json(refuse(`This person owns "${shared.name}", which has other members. Transfer that workspace first.`), 400);
  const organizationIds = owned.rows.map((row) => row.id as string);
  const client = await pool.connect();
  let storageKeys: string[] = [];
  try {
    await client.query("BEGIN");
    if (organizationIds.length) {
      const blobs = await client.query("SELECT storage_key FROM vault_blobs WHERE organization_id = ANY($1)", [organizationIds]);
      const temps = await client.query("SELECT temp_storage_key FROM vault_upload_sessions WHERE organization_id = ANY($1)", [organizationIds]);
      storageKeys = [...blobs.rows.map((row) => row.storage_key as string), ...temps.rows.map((row) => row.temp_storage_key as string)];
      await client.query("DELETE FROM organization WHERE id = ANY($1)", [organizationIds]);
    }
    await client.query('DELETE FROM "user" WHERE id = $1', [body.userId]);
    await client.query("COMMIT");
  } catch (error) {
    await client.query("ROLLBACK"); throw error;
  } finally { client.release(); }
  return c.json(data({ ok: true, storageKeys }));
});

app.post("/internal/admin/reset-link", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ requesterId: string; userId: string }>();
  try { await requireAdmin(body.requesterId); } catch { return c.json({ message: "Forbidden" }, 403); }
  const target = await pool.query('SELECT id, email FROM "user" WHERE id = $1', [body.userId]);
  if (!target.rowCount) return c.json({ message: "Not found" }, 404);
  await auth.api.requestPasswordReset({ body: { email: target.rows[0].email, redirectTo: publicBaseURL + "/reset-password" } });
  const captured = pendingResetTokens.get(body.userId); pendingResetTokens.delete(body.userId);
  if (!captured || captured.expiresAt < Date.now()) return c.json({ message: "Reset link could not be created" }, 500);
  return c.json(data({ url: publicBaseURL + "/reset-password?token=" + encodeURIComponent(captured.token) }));
});

app.post("/internal/workspaces/join", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ organizationId: string; userId: string; role: "admin" | "member" }>();
  const user = await pool.query('SELECT "approvalStatus" FROM "user" WHERE id = $1', [body.userId]);
  if (user.rows[0]?.approvalStatus !== "approved") return c.json({ message: "Account is not approved" }, 403);
  if (!(await membership(body.userId, body.organizationId))) {
    await auth.api.addMember({ body: { organizationId: body.organizationId, userId: body.userId, role: body.role } });
  }
  return c.json(data({ ok: true }));
});

app.get("/internal/workspaces/details", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const organizationId = c.req.query("organizationId") || "";
  const requesterId = c.req.query("requesterId") || "";
  const role = await membership(requesterId, organizationId);
  if (!role) return c.json({ message: "Forbidden" }, 403);
  const members = await pool.query(
    'SELECT u.id, u.name, u.email, m.role, m."createdAt" FROM member m JOIN "user" u ON u.id = m."userId" WHERE m."organizationId" = $1 ORDER BY CASE m.role WHEN \'owner\' THEN 0 WHEN \'admin\' THEN 1 ELSE 2 END, m."createdAt"',
    [organizationId],
  );
  const settings = await pool.query(
    'SELECT max_upload_bytes AS "maxUploadBytes" FROM vault_workspace_settings WHERE organization_id = $1',
    [organizationId],
  );
  return c.json(data({
    members: ["owner", "admin"].includes(role) ? members.rows : members.rows.filter((row) => row.id === requesterId),
    maxUploadBytes: Number(settings.rows[0]?.maxUploadBytes || 10737418240),
  }));
});

app.post("/internal/workspaces/member-role", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ organizationId: string; requesterId: string; userId: string; role: string }>();
  const requesterRole = await membership(body.requesterId, body.organizationId);
  if (!requesterRole || !["owner", "admin"].includes(requesterRole)) return c.json({ message: "Forbidden" }, 403);
  if (!['member', 'admin'].includes(body.role)) return c.json({ message: "Invalid role" }, 400);
  const target = await membership(body.userId, body.organizationId);
  if (!target) return c.json({ message: "Not found" }, 404);
  if (target === "owner") return c.json({ message: "Transfer ownership instead" }, 403);
  await pool.query('UPDATE member SET role = $1 WHERE "organizationId" = $2 AND "userId" = $3', [body.role, body.organizationId, body.userId]);
  return c.json(data({ ok: true }));
});

app.post("/internal/workspaces/remove-member", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ organizationId: string; requesterId: string; userId: string }>();
  const requesterRole = await membership(body.requesterId, body.organizationId);
  if (!requesterRole || !["owner", "admin"].includes(requesterRole)) return c.json({ message: "Forbidden" }, 403);
  const target = await membership(body.userId, body.organizationId);
  if (!target) return c.json({ message: "Not found" }, 404);
  if (target === "owner") return c.json({ message: "The owner cannot be removed" }, 403);
  await pool.query('DELETE FROM member WHERE "organizationId" = $1 AND "userId" = $2', [body.organizationId, body.userId]);
  return c.json(data({ ok: true }));
});

app.post("/internal/workspaces/transfer", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ organizationId: string; requesterId: string; userId: string }>();
  if (await membership(body.requesterId, body.organizationId) !== "owner") return c.json({ message: "Forbidden" }, 403);
  if (body.requesterId === body.userId || !(await membership(body.userId, body.organizationId))) return c.json({ message: "Invalid new owner" }, 400);
  const client = await pool.connect();
  try {
    await client.query("BEGIN");
    await client.query('SELECT id FROM member WHERE "organizationId" = $1 FOR UPDATE', [body.organizationId]);
    await client.query('UPDATE member SET role = \'admin\' WHERE "organizationId" = $1 AND "userId" = $2', [body.organizationId, body.requesterId]);
    await client.query('UPDATE member SET role = \'owner\' WHERE "organizationId" = $1 AND "userId" = $2', [body.organizationId, body.userId]);
    await client.query("COMMIT");
  } catch (error) {
    await client.query("ROLLBACK"); throw error;
  } finally { client.release(); }
  return c.json(data({ ok: true }));
});

app.post("/internal/workspaces/delete", async (c) => {
  if (!requireBridge(c.req.header("x-auth-bridge-secret"))) return c.json({ message: "Unauthorized" }, 401);
  const body = await c.req.json<{ organizationId: string; requesterId: string; confirmation: string }>();
  if (await membership(body.requesterId, body.organizationId) !== "owner") return c.json({ message: "Forbidden" }, 403);
  const organization = await pool.query('SELECT name FROM organization WHERE id = $1', [body.organizationId]);
  if (!organization.rowCount || body.confirmation !== organization.rows[0].name) return c.json({ message: "Workspace name does not match" }, 400);
  const files = await pool.query('SELECT storage_key FROM vault_blobs WHERE organization_id = $1', [body.organizationId]);
  await pool.query('DELETE FROM organization WHERE id = $1', [body.organizationId]);
  return c.json(data({ ok: true, storageKeys: files.rows.map((row) => row.storage_key as string) }));
});

app.on(["POST", "GET"], "/api/auth/*", (c) => auth.handler(c.req.raw));
serve({ fetch: app.fetch, port: Number(process.env.AUTH_PORT || 3001), hostname: "0.0.0.0" }, (info) => {
  console.log(JSON.stringify({ level: "info", message: "auth service started", port: info.port }));
});
