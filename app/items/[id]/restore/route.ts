import { eq } from "drizzle-orm";
import { clipboardItems } from "../../../../db/schema";
import {
  errorResponse,
  logActivity,
  readyDb,
  requireApiUser,
  serializeItem,
} from "../../../api-utils";

type RouteContext = { params: Promise<{ id: string }> };

export async function POST(request: Request, context: RouteContext) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const { id } = await context.params;
  const db = await readyDb();
  const [item] = await db
    .update(clipboardItems)
    .set({ deletedAt: null })
    .where(eq(clipboardItems.id, id))
    .returning();
  if (!item) return errorResponse("NOT_FOUND", "Clipboard item not found", 404);
  await logActivity(request, "restore", id);
  return Response.json({ item: serializeItem(item) });
}
