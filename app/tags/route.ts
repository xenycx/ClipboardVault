import { isNull } from "drizzle-orm";
import { clipboardItems } from "../../db/schema";
import {
  errorResponse,
  parseTags,
  readyDb,
  requireApiUser,
} from "../api-utils";

export async function GET(request: Request) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const q = new URL(request.url).searchParams.get("q")?.trim().toLowerCase() ?? "";
  const db = await readyDb();
  const rows = await db
    .select({ tags: clipboardItems.tags })
    .from(clipboardItems)
    .where(isNull(clipboardItems.deletedAt));
  const tags = Array.from(new Set(rows.flatMap((row) => parseTags(row.tags))))
    .filter((tag) => tag.includes(q))
    .sort()
    .slice(0, 20);
  return Response.json({ tags });
}
