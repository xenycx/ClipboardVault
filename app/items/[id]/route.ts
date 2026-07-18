import { and, eq, isNull, sql } from "drizzle-orm";
import { clipboardItems } from "../../../db/schema";
import {
  errorResponse,
  logActivity,
  normalizeTags,
  readyDb,
  requireApiUser,
  serializeItem,
} from "../../api-utils";

type RouteContext = { params: Promise<{ id: string }> };

export async function GET(_: Request, context: RouteContext) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const { id } = await context.params;
  const db = await readyDb();
  const [item] = await db
    .select()
    .from(clipboardItems)
    .where(eq(clipboardItems.id, id))
    .limit(1);
  if (!item) return errorResponse("NOT_FOUND", "Clipboard item not found", 404);
  return Response.json({ item: serializeItem(item) });
}

export async function PATCH(request: Request, context: RouteContext) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const { id } = await context.params;
  let body: Record<string, unknown>;
  try {
    body = (await request.json()) as Record<string, unknown>;
  } catch {
    return errorResponse("VALIDATION_ERROR", "Request body must be JSON", 400);
  }

  const db = await readyDb();
  const changes: Partial<typeof clipboardItems.$inferInsert> = {};
  if (typeof body.pinned === "boolean") {
    if (body.pinned) {
      const [result] = await db
        .select({ count: sql<number>`count(*)` })
        .from(clipboardItems)
        .where(and(eq(clipboardItems.pinned, true), isNull(clipboardItems.deletedAt)));
      if (Number(result?.count ?? 0) >= 10) {
        return errorResponse("VALIDATION_ERROR", "You can pin up to 10 items", 400);
      }
    }
    changes.pinned = body.pinned;
  }
  if (Array.isArray(body.tags)) changes.tags = JSON.stringify(normalizeTags(body.tags));
  if (typeof body.source_url === "string" || body.source_url === null) {
    const sourceUrl = typeof body.source_url === "string" ? body.source_url.trim() : null;
    if (sourceUrl && sourceUrl.length > 2048) {
      return errorResponse("VALIDATION_ERROR", "Source URL is too long", 400);
    }
    changes.sourceUrl = sourceUrl;
  }
  if (!Object.keys(changes).length) {
    return errorResponse("VALIDATION_ERROR", "No supported fields supplied", 400);
  }

  const [item] = await db
    .update(clipboardItems)
    .set(changes)
    .where(eq(clipboardItems.id, id))
    .returning();
  if (!item) return errorResponse("NOT_FOUND", "Clipboard item not found", 404);
  await logActivity(request, "update", id, { fields: Object.keys(changes) });
  return Response.json({ item: serializeItem(item) });
}

export async function DELETE(request: Request, context: RouteContext) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const { id } = await context.params;
  const permanent = new URL(request.url).searchParams.get("permanent") === "true";
  const db = await readyDb();
  const rows = permanent
    ? await db.delete(clipboardItems).where(eq(clipboardItems.id, id)).returning({ id: clipboardItems.id })
    : await db
        .update(clipboardItems)
        .set({ deletedAt: new Date().toISOString(), pinned: false })
        .where(eq(clipboardItems.id, id))
        .returning({ id: clipboardItems.id });
  if (!rows.length) return errorResponse("NOT_FOUND", "Clipboard item not found", 404);
  await logActivity(request, permanent ? "permanent_delete" : "delete", id);
  return new Response(null, { status: 204 });
}
