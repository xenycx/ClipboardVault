import { betterAuth } from "better-auth";
import { admin, createAccessControl, organization } from "better-auth/plugins";
import {
  adminAc,
  defaultStatements,
  memberAc,
  ownerAc,
} from "better-auth/plugins/organization/access";
import { apiKey } from "@better-auth/api-key";
import { Pool } from "pg";

export const pool = new Pool({
  connectionString: process.env.DATABASE_URL,
  max: Number(process.env.AUTH_DATABASE_MAX_CONNECTIONS || 10),
});

const baseURL = (process.env.BETTER_AUTH_URL || "http://localhost:8080").replace(/\/$/, "");
const googleConfigured = Boolean(process.env.GOOGLE_CLIENT_ID && process.env.GOOGLE_CLIENT_SECRET);
export const pendingResetTokens = new Map<string, { token: string; expiresAt: number }>();

const workspaceStatements = {
  ...defaultStatements,
  apiKey: ["create", "read", "update", "delete"],
} as const;
const workspaceAccess = createAccessControl(workspaceStatements);
const workspaceRoles = {
  owner: workspaceAccess.newRole({
    ...ownerAc.statements,
    apiKey: ["create", "read", "update", "delete"],
  }),
  admin: workspaceAccess.newRole({
    ...adminAc.statements,
    apiKey: ["create", "read", "update", "delete"],
  }),
  member: workspaceAccess.newRole({
    ...memberAc.statements,
    apiKey: ["create"],
  }),
};

export const auth = betterAuth({
  appName: "Clipboard Vault",
  baseURL,
  secret: process.env.BETTER_AUTH_SECRET,
  database: pool,
  trustedOrigins: [baseURL],
  emailAndPassword: {
    enabled: true,
    autoSignIn: true,
    requireEmailVerification: false,
    minPasswordLength: 8,
    maxPasswordLength: 128,
    revokeSessionsOnPasswordReset: true,
    resetPasswordTokenExpiresIn: 15 * 60,
    async sendResetPassword({ user, token }) {
      pendingResetTokens.set(user.id, { token, expiresAt: Date.now() + 60_000 });
    },
  },
  socialProviders: googleConfigured ? {
    google: {
      clientId: process.env.GOOGLE_CLIENT_ID!,
      clientSecret: process.env.GOOGLE_CLIENT_SECRET!,
    },
  } : {},
  user: {
    additionalFields: {
      approvalStatus: { type: "string", required: false, defaultValue: "pending", input: false },
      approvedAt: { type: "date", required: false, input: false },
      approvedBy: { type: "string", required: false, input: false },
    },
  },
  session: {
    expiresIn: 60 * 60 * 24 * 7,
    updateAge: 60 * 60 * 24,
    cookieCache: { enabled: true, maxAge: 60 },
  },
  advanced: {
    useSecureCookies: baseURL.startsWith("https://"),
    defaultCookieAttributes: {
      httpOnly: true,
      secure: baseURL.startsWith("https://"),
      sameSite: "lax",
    },
  },
  rateLimit: {
    enabled: true,
    window: 60,
    max: 100,
    customRules: {
      "/sign-in/email": { window: 60, max: 10 },
      "/sign-up/email": { window: 300, max: 5 },
      "/request-password-reset": { window: 300, max: 3 },
    },
  },
  plugins: [
    admin({ defaultRole: "user", adminRoles: ["admin"] }),
    organization({
      ac: workspaceAccess,
      roles: workspaceRoles,
      allowUserToCreateOrganization: async (user) => {
        const result = await pool.query('SELECT "approvalStatus" FROM "user" WHERE id = $1', [user.id]);
        return result.rows[0]?.approvalStatus === "approved";
      },
      invitationExpiresIn: 48 * 60 * 60,
      membershipLimit: 100,
      requireEmailVerificationOnInvitation: false,
    }),
    apiKey({
      references: "organization",
      enableMetadata: true,
      defaultPrefix: "cv_live_",
      keyExpiration: { defaultExpiresIn: 90 * 24 * 60 * 60, maxExpiresIn: 3650 },
      rateLimit: { enabled: true, timeWindow: 60_000, maxRequests: 120 },
    }),
  ],
});

export const publicBaseURL = baseURL;
