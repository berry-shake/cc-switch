export type CredentialStatus =
  | "valid"
  | "expired"
  | "not_found"
  | "parse_error";

export interface QuotaTier {
  name: string;
  /** 服务端返回的额度窗口长度（秒）；当前仅 Codex 明确提供。 */
  windowSeconds?: number | null;
  utilization: number; // 0-100
  resetsAt: string | null;
  usedValueUsd?: number | null;
  maxValueUsd?: number | null;
  planLabel?: string | null;
}

export interface ExtraUsage {
  isEnabled: boolean;
  monthlyLimit: number | null;
  usedCredits: number | null;
  utilization: number | null;
  currency: string | null;
}

export interface SubscriptionQuota {
  tool: string;
  credentialStatus: CredentialStatus;
  credentialMessage: string | null;
  success: boolean;
  tiers: QuotaTier[];
  extraUsage: ExtraUsage | null;
  error: string | null;
  queriedAt: number | null;
}

export type CodexAnalyticsAccountMode = "workspace" | "personal";

export interface CodexAnalyticsTokenCounts {
  uncachedInputTokens: number;
  cachedInputTokens: number;
  cacheWriteInputTokens: number;
  outputTokens: number;
  totalTokens: number;
}

export interface CodexAnalyticsModelUsage {
  model: string;
  speed: string;
  credits: number;
  tokens: CodexAnalyticsTokenCounts;
}

export interface CodexAnalyticsDailyUsage {
  date: string;
  totals: CodexAnalyticsTokenCounts;
  models: CodexAnalyticsModelUsage[];
}

/**
 * Codex Web analytics 的脱敏结果。OAuth token 和 account id 始终留在
 * Rust 侧，前端只接收计算所需的每日 Token 与模型/速度信息。
 */
export interface CodexAnalyticsUsage {
  accountMode: CodexAnalyticsAccountMode;
  days: CodexAnalyticsDailyUsage[];
  queriedAt: number;
}

export type CodexCredentialSource = "file" | "keyring";

export interface CodexQuotaSnapshot {
  quota: SubscriptionQuota;
  /** 与本次官方额度响应属于同一账号；接口未返回时为 null。 */
  email: string | null;
  credentialSource: CodexCredentialSource;
  /** SHA-256 派生的匿名账号域，不包含 account id 或 token。 */
  credentialScope: string;
}

export interface CodexOfficialUsageSnapshot extends CodexQuotaSnapshot {
  analytics: CodexAnalyticsUsage | null;
  queriedAt: number;
}
