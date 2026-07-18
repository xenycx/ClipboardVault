import { eq } from "drizzle-orm";
import { vaultSettings } from "../../db/schema";
import {
  errorResponse,
  logActivity,
  readyDb,
  requireApiUser,
} from "../api-utils";

const DEFAULTS = {
  max_items: 10_000,
  max_age_days: 365,
  max_total_size_mb: 500,
  action_on_limit: "delete_oldest",
};

export async function GET() {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const db = await readyDb();
  const [row] = await db
    .select()
    .from(vaultSettings)
    .where(eq(vaultSettings.key, "retention"))
    .limit(1);
  return Response.json({ settings: row ? JSON.parse(row.value) : DEFAULTS });
}

export async function PUT(request: Request) {
  if (!(await requireApiUser())) {
    return errorResponse("TOKEN_INVALID", "Authentication required", 401);
  }
  const body = (await request.json()) as Record<string, unknown>;
  const settings = {
    max_items: Math.min(100_000, Math.max(100, Number(body.max_items) || DEFAULTS.max_items)),
    max_age_days: Math.min(3650, Math.max(1, Number(body.max_age_days) || DEFAULTS.max_age_days)),
    max_total_size_mb: Math.min(10_000, Math.max(10, Number(body.max_total_size_mb) || DEFAULTS.max_total_size_mb)),
    action_on_limit: body.action_on_limit === "reject_new" ? "reject_new" : "delete_oldest",
  };
  const db = await readyDb();
  await db
    .insert(vaultSettings)
    .values({ key: "retention", value: JSON.stringify(settings) })
    .onConflictDoUpdate({
      target: vaultSettings.key,
      set: { value: JSON.stringify(settings), updatedAt: new Date().toISOString() },
    });
  await logActivity(request, "settings_update", null, settings);
  return Response.json({ settings });
}
