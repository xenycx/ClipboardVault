import { and, desc, inArray, isNull } from "drizzle-orm";
import { clipboardItems } from "../../../db/schema";
import {
  errorResponse,
  logActivity,
  readyDb,
  requireApiUser,
  serializeItem,
} from "../../api-utils";

function csvCell(value: unknown) {
  const text = String(value ?? "");
  return `"${text.replaceAll('"', '""')}"`;
}

export async function GET(request: Request) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const url = new URL(request.url);
  const format = url.searchParams.get("format") ?? "json";
  const ids = (url.searchParams.get("ids") ?? "")
    .split(",")
    .map((id) => id.trim())
    .filter(Boolean);
  if (!["json", "jsonl", "csv"].includes(format)) {
    return errorResponse("VALIDATION_ERROR", "Unsupported export format", 400);
  }

  const conditions = [isNull(clipboardItems.deletedAt)];
  if (ids.length) conditions.push(inArray(clipboardItems.id, ids));
  const db = await readyDb();
  const rows = await db
    .select()
    .from(clipboardItems)
    .where(and(...conditions))
    .orderBy(desc(clipboardItems.createdAt));
  const items = rows.map(serializeItem);

  let content: string;
  let contentType: string;
  let extension: string;
  if (format === "jsonl") {
    content = items.map((item) => JSON.stringify(item)).join("\n");
    contentType = "application/x-ndjson; charset=utf-8";
    extension = "jsonl";
  } else if (format === "csv") {
    const header = "id,type,size,created_at,payload_preview";
    const lines = items.map((item) =>
      [
        csvCell(item.id),
        csvCell(item.type),
        csvCell(item.sizeBytes),
        csvCell(item.createdAt),
        csvCell(item.payload.slice(0, 180).replaceAll("\n", " ")),
      ].join(","),
    );
    content = [header, ...lines].join("\n");
    contentType = "text/csv; charset=utf-8";
    extension = "csv";
  } else {
    content = JSON.stringify({ exported_at: new Date().toISOString(), items }, null, 2);
    contentType = "application/json; charset=utf-8";
    extension = "json";
  }

  await logActivity(request, "export", null, { format, count: items.length });
  return new Response(content, {
    headers: {
      "content-type": contentType,
      "content-disposition": `attachment; filename="clipboard-export.${extension}"`,
      "cache-control": "no-store",
    },
  });
}
