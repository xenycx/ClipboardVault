import type { Metadata } from "next";
import { requireChatGPTUser } from "./chatgpt-auth";
import { VaultApp } from "./VaultApp";

export const dynamic = "force-dynamic";

export const metadata: Metadata = {
  title: "Vault",
  description: "Private, searchable memory for everything you copy.",
};

export default async function Home() {
  const user =
    process.env.NODE_ENV === "development"
      ? { displayName: "Alex", email: "preview@local.invalid", fullName: "Alex" }
      : await requireChatGPTUser("/");
  return <VaultApp displayName={user.displayName} />;
}
