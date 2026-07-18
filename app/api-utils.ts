import { getChatGPTUser } from "./chatgpt-auth";
import { getDb } from "../db";
import { activityLog } from "../db/schema";
import { ensureVaultSchema } from "../db/init";

export const ITEM_TYPES = ["text", "html", "code", "url", "image"] as const;
export type ItemType = (typeof ITEM_TYPES)[number];

export function errorResponse(
  code: string,
  message: string,
  status: number,
  detail?: Record<string, unknown>,
) {
  return Response.json(
    { error: { code, message, ...(detail ? { detail } : {}) } },
    { status },
  );
}

export async function requireApiUser() {
  const user = await getChatGPTUser();
  if (user) return user;
  if (process.env.NODE_ENV === "development") {
    return {
      displayName: "Local preview",
      email: "preview@local.invalid",
      fullName: "Local preview",
    };
  }
  return null;
}

export async function readyDb() {
  await ensureVaultSchema();
  return getDb();
}

export function requestIp(request: Request) {
  return (
    request.headers.get("cf-connecting-ip") ??
    request.headers.get("x-forwarded-for")?.split(",")[0]?.trim() ??
    "unknown"
  );
}

export async function logActivity(
  request: Request,
  action: string,
  itemId?: string | null,
  detail?: Record<string, unknown>,
) {
  const db = await readyDb();
  await db.insert(activityLog).values({
    action,
    itemId: itemId ?? null,
    detail: detail ? JSON.stringify(detail) : null,
    ip: requestIp(request),
  });
}

export async function sha256(value: string) {
  const bytes = new TextEncoder().encode(value);
  const hash = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(hash), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

export function parseTags(value: string | null | undefined) {
  if (!value) return [] as string[];
  try {
    const tags = JSON.parse(value);
    return Array.isArray(tags)
      ? tags.filter((tag): tag is string => typeof tag === "string")
      : [];
  } catch {
    return [];
  }
}

export function normalizeTags(value: unknown) {
  if (!Array.isArray(value)) return [];
  return Array.from(
    new Set(
      value
        .filter((tag): tag is string => typeof tag === "string")
        .map((tag) => tag.trim().toLowerCase())
        .filter(Boolean)
        .slice(0, 12),
    ),
  );
}

export function serializeItem<T extends { tags: string }>(item: T) {
  return { ...item, tags: parseTags(item.tags) };
}
