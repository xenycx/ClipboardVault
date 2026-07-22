type OAuthTokens = { accessToken?: string };
type ProviderUser = {
  id: string;
  name: string;
  email: string;
  emailVerified: true;
  image?: string;
};
type ProviderUserInfo = { user: ProviderUser; data: unknown };

type GitHubEmail = { email?: unknown; primary?: unknown; verified?: unknown };
export function selectVerifiedGitHubEmail(value: unknown): string | null {
  if (!Array.isArray(value)) return null;
  const emails = value.filter((entry): entry is GitHubEmail => Boolean(entry) && typeof entry === "object");
  const selected = emails.find((entry) => entry.primary === true && entry.verified === true)
    ?? emails.find((entry) => entry.verified === true);
  return typeof selected?.email === "string" && selected.email.trim() ? selected.email : null;
}

export function selectVerifiedDiscordEmail(value: unknown): string | null {
  if (!value || typeof value !== "object") return null;
  const profile = value as { email?: unknown; verified?: unknown };
  return profile.verified === true && typeof profile.email === "string" && profile.email.trim()
    ? profile.email
    : null;
}

async function getGitHubUserInfo(tokens: OAuthTokens): Promise<ProviderUserInfo | null> {
  if (!tokens.accessToken) return null;
  const headers = {
    accept: "application/vnd.github+json",
    authorization: `Bearer ${tokens.accessToken}`,
    "user-agent": "Clipboard-Vault",
    "x-github-api-version": "2022-11-28",
  };
  const [profileResponse, emailsResponse] = await Promise.all([
    fetch("https://api.github.com/user", { headers }),
    fetch("https://api.github.com/user/emails", { headers }),
  ]);
  if (!profileResponse.ok || !emailsResponse.ok) return null;
  const profile = await profileResponse.json() as Record<string, unknown>;
  const email = selectVerifiedGitHubEmail(await emailsResponse.json());
  if (!email || (typeof profile.id !== "number" && typeof profile.id !== "string")) return null;
  const login = typeof profile.login === "string" ? profile.login : "GitHub user";
  return {
    user: {
      id: String(profile.id),
      name: typeof profile.name === "string" && profile.name.trim() ? profile.name : login,
      email,
      emailVerified: true,
      ...(typeof profile.avatar_url === "string" ? { image: profile.avatar_url } : {}),
    },
    data: profile,
  };
}

async function getDiscordUserInfo(tokens: OAuthTokens): Promise<ProviderUserInfo | null> {
  if (!tokens.accessToken) return null;
  const response = await fetch("https://discord.com/api/users/@me", {
    headers: { authorization: `Bearer ${tokens.accessToken}` },
  });
  if (!response.ok) return null;
  const profile = await response.json() as Record<string, unknown>;
  const email = selectVerifiedDiscordEmail(profile);
  if (!email || typeof profile.id !== "string") return null;
  const username = typeof profile.username === "string" ? profile.username : "Discord user";
  const name = typeof profile.global_name === "string" && profile.global_name.trim()
    ? profile.global_name
    : username;
  const image = typeof profile.avatar === "string"
    ? `https://cdn.discordapp.com/avatars/${profile.id}/${profile.avatar}.png`
    : undefined;
  return {
    user: { id: profile.id, name, email, emailVerified: true, ...(image ? { image } : {}) },
    data: profile,
  };
}

export const providerDefinitions = [
  {
    id: "google",
    label: "Google",
    clientIdVariable: "GOOGLE_CLIENT_ID",
    clientSecretVariable: "GOOGLE_CLIENT_SECRET",
    scopes: ["openid", "email", "profile"],
  },
  {
    id: "github",
    label: "GitHub",
    clientIdVariable: "GITHUB_CLIENT_ID",
    clientSecretVariable: "GITHUB_CLIENT_SECRET",
    scopes: ["read:user", "user:email"],
    getUserInfo: getGitHubUserInfo,
  },
  {
    id: "discord",
    label: "Discord",
    clientIdVariable: "DISCORD_CLIENT_ID",
    clientSecretVariable: "DISCORD_CLIENT_SECRET",
    scopes: ["identify", "email"],
    getUserInfo: getDiscordUserInfo,
  },
] as const;

export type SocialProviderId = (typeof providerDefinitions)[number]["id"];
export type PublicSocialProvider = { id: SocialProviderId; label: string };
type ProviderEnvironment = Record<string, string | undefined>;
type SocialProviderConfiguration = {
  clientId: string;
  clientSecret: string;
  scope: string[];
  disableDefaultScope: true;
  getUserInfo?: (tokens: OAuthTokens) => Promise<ProviderUserInfo | null>;
};

export function buildProviderRegistry(environment: ProviderEnvironment) {
  const socialProviders: Partial<Record<SocialProviderId, SocialProviderConfiguration>> = {};
  const publicProviders: PublicSocialProvider[] = [];

  for (const definition of providerDefinitions) {
    const clientId = environment[definition.clientIdVariable]?.trim() || "";
    const clientSecret = environment[definition.clientSecretVariable]?.trim() || "";

    if (Boolean(clientId) !== Boolean(clientSecret)) {
      const missing = clientId ? definition.clientSecretVariable : definition.clientIdVariable;
      throw new Error(
        `${definition.label} OAuth configuration is incomplete: ${missing} must be set when ` +
        `${clientId ? definition.clientIdVariable : definition.clientSecretVariable} is set`,
      );
    }
    if (!clientId) continue;

    const configuration: SocialProviderConfiguration = {
      clientId,
      clientSecret,
      scope: [...definition.scopes],
      disableDefaultScope: true,
    };
    if ("getUserInfo" in definition) configuration.getUserInfo = definition.getUserInfo;
    socialProviders[definition.id] = configuration;
    publicProviders.push({ id: definition.id, label: definition.label });
  }

  return { socialProviders, publicProviders };
}

export const providerRegistry = buildProviderRegistry(process.env);
