import { desc } from "drizzle-orm";
import { activityLog } from "../../../db/schema";
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
  const activity = await db
    .select()
    .from(activityLog)
    .orderBy(desc(activityLog.createdAt), desc(activityLog.id))
    .limit(200);
  return Response.json({ activity });
}
