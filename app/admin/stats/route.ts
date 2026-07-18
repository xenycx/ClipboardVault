import { desc, isNull, sql } from "drizzle-orm";
import { clipboardItems } from "../../../db/schema";
import {
  errorResponse,
  readyDb,
  requireApiUser,
} from "../../api-utils";

export async function GET() {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const db = await readyDb();
  const active = isNull(clipboardItems.deletedAt);
  const [totals] = await db
    .select({
      totalItems: sql<number>`count(*)`,
      totalSizeBytes: sql<number>`coalesce(sum(${clipboardItems.sizeBytes}), 0)`,
      pinnedItems: sql<number>`coalesce(sum(case when ${clipboardItems.pinned} = 1 then 1 else 0 end), 0)`,
    })
    .from(clipboardItems)
    .where(active);
  const typeRows = await db
    .select({ type: clipboardItems.type, count: sql<number>`count(*)` })
    .from(clipboardItems)
    .where(active)
    .groupBy(clipboardItems.type);
  const dailyRows = await db
    .select({
      date: sql<string>`date(${clipboardItems.createdAt})`,
      count: sql<number>`count(*)`,
    })
    .from(clipboardItems)
    .where(active)
    .groupBy(sql`date(${clipboardItems.createdAt})`)
    .orderBy(desc(sql`date(${clipboardItems.createdAt})`))
    .limit(30);

  const totalItems = Number(totals?.totalItems ?? 0);
  const totalSizeBytes = Number(totals?.totalSizeBytes ?? 0);
  return Response.json({
    total_items: totalItems,
    total_size_bytes: totalSizeBytes,
    pinned_items: Number(totals?.pinnedItems ?? 0),
    type_breakdown: Object.fromEntries(
      typeRows.map((row) => [row.type, Number(row.count)]),
    ),
    daily_counts: dailyRows.map((row) => ({
      date: row.date,
      count: Number(row.count),
    })),
    avg_payload_size: totalItems ? Math.floor(totalSizeBytes / totalItems) : 0,
  });
}
