import type { Metadata } from "next";
import { requireChatGPTUser } from "../chatgpt-auth";
import { VaultApp } from "../VaultApp";

export const dynamic = "force-dynamic";

export const metadata: Metadata = {
  title: "Overview",
};

export default async function AdminPage() {
  const user =
    process.env.NODE_ENV === "development"
      ? { displayName: "Alex", email: "preview@local.invalid", fullName: "Alex" }
      : await requireChatGPTUser("/admin");
  return <VaultApp initialSection="dashboard" displayName={user.displayName} />;
}
