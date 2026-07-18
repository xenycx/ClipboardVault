import {
  and,
  desc,
  eq,
  inArray,
  isNull,
  like,
  lt,
} from "drizzle-orm";
import { clipboardItems } from "../../db/schema";
import {
  errorResponse,
  ITEM_TYPES,
  logActivity,
  normalizeTags,
  readyDb,
  requireApiUser,
  serializeItem,
  sha256,
} from "../api-utils";

const MAX_PAYLOAD_BYTES = 524_288;

export async function GET(request: Request) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }

  const url = new URL(request.url);
  const q = url.searchParams.get("q")?.trim();
  const type = url.searchParams.get("type");
  const before = url.searchParams.get("before");
  const includeTrash = url.searchParams.get("trash") === "true";
  const limit = Math.min(
    200,
    Math.max(1, Number(url.searchParams.get("limit") ?? 50) || 50),
  );
  const requestedTags = (url.searchParams.get("tags") ?? "")
    .split(",")
    .map((tag) => tag.trim().toLowerCase())
    .filter(Boolean);
  const tagsMode = url.searchParams.get("tags_mode") === "or" ? "or" : "and";

  const conditions = [
    includeTrash
      ? lt(clipboardItems.deletedAt, "9999-12-31")
      : isNull(clipboardItems.deletedAt),
  ];
  if (q) conditions.push(like(clipboardItems.payload, `%${q}%`));
  if (type && ITEM_TYPES.includes(type as (typeof ITEM_TYPES)[number])) {
    conditions.push(
      eq(clipboardItems.type, type as (typeof ITEM_TYPES)[number]),
    );
  }
  if (before) conditions.push(lt(clipboardItems.createdAt, before));

  const db = await readyDb();
  const rows = await db
    .select()
    .from(clipboardItems)
    .where(and(...conditions))
    .orderBy(desc(clipboardItems.pinned), desc(clipboardItems.createdAt), desc(clipboardItems.id))
    .limit(limit + 1);

  const tagged = requestedTags.length
    ? rows.filter((row) => {
        const tags = serializeItem(row).tags;
        return tagsMode === "or"
          ? requestedTags.some((tag) => tags.includes(tag))
          : requestedTags.every((tag) => tags.includes(tag));
      })
    : rows;
  const hasMore = tagged.length > limit;
  const items = tagged.slice(0, limit).map(serializeItem);

  return Response.json({
    items,
    next_cursor: hasMore ? items.at(-1)?.createdAt ?? null : null,
  });
}

export async function POST(request: Request) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }

  let body: Record<string, unknown>;
  try {
    body = (await request.json()) as Record<string, unknown>;
  } catch {
    return errorResponse("VALIDATION_ERROR", "Request body must be JSON", 400);
  }

  const payload = typeof body.payload === "string" ? body.payload : "";
  const type = typeof body.type === "string" ? body.type : "text";
  const sourceUrl =
    typeof body.source_url === "string" ? body.source_url.trim() : null;
  const sizeBytes = new TextEncoder().encode(payload).byteLength;

  if (!payload.trim()) {
    return errorResponse(
      "VALIDATION_ERROR",
      "Payload cannot be blank or whitespace only",
      400,
    );
  }
  if (sizeBytes > MAX_PAYLOAD_BYTES) {
    return errorResponse(
      "PAYLOAD_TOO_LARGE",
      `Payload exceeds maximum allowed size of ${MAX_PAYLOAD_BYTES} bytes`,
      413,
      { received: sizeBytes, limit: MAX_PAYLOAD_BYTES },
    );
  }
  if (!ITEM_TYPES.includes(type as (typeof ITEM_TYPES)[number])) {
    return errorResponse("VALIDATION_ERROR", "Unsupported clipboard type", 400);
  }
  if (sourceUrl && sourceUrl.length > 2048) {
    return errorResponse("VALIDATION_ERROR", "Source URL is too long", 400);
  }

  const contentHash =
    typeof body.content_hash === "string" && /^[a-f0-9]{64}$/i.test(body.content_hash)
      ? body.content_hash.toLowerCase()
      : await sha256(payload);
  const db = await readyDb();
  const [item] = await db
    .insert(clipboardItems)
    .values({
      id: crypto.randomUUID(),
      payload,
      type: type as (typeof clipboardItems.$inferInsert)["type"],
      sourceUrl,
      contentHash,
      sizeBytes,
      tags: JSON.stringify(normalizeTags(body.tags)),
    })
    .onConflictDoNothing({ target: clipboardItems.contentHash })
    .returning();

  if (!item) {
    return errorResponse(
      "DUPLICATE_HASH",
      "This clipboard item is already stored",
      409,
      { content_hash: contentHash },
    );
  }

  await logActivity(request, "create", item.id, { type: item.type });
  return Response.json({ item: serializeItem(item) }, { status: 201 });
}

export async function DELETE(request: Request) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }

  const url = new URL(request.url);
  const ids = url.searchParams
    .getAll("ids")
    .flatMap((entry) => entry.split(","))
    .map((id) => id.trim())
    .filter(Boolean);
  const clearAll = url.searchParams.get("clear_all") === "true";
  const olderThanDays = Number(url.searchParams.get("older_than_days"));

  if (!ids.length && !clearAll && !Number.isFinite(olderThanDays)) {
    return errorResponse(
      "VALIDATION_ERROR",
      "Provide ids, older_than_days, or clear_all=true",
      400,
    );
  }

  const conditions = [isNull(clipboardItems.deletedAt)];
  if (ids.length) conditions.push(inArray(clipboardItems.id, ids));
  if (Number.isFinite(olderThanDays) && olderThanDays > 0) {
    const cutoff = new Date(Date.now() - olderThanDays * 86_400_000).toISOString();
    conditions.push(lt(clipboardItems.createdAt, cutoff));
  }

  const db = await readyDb();
  const deleted = await db
    .update(clipboardItems)
    .set({ deletedAt: new Date().toISOString(), pinned: false })
    .where(and(...conditions))
    .returning({ id: clipboardItems.id });

  await logActivity(request, "bulk_delete", null, { count: deleted.length });
  return Response.json({ deleted_count: deleted.length });
}
