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
  /** 该日有正向模型额度，但总 Token 日报尚无正向记录。 */
  missingTokenData: boolean;
  /** 该日有正向总 Token，但模型/速度额度明细尚无正向记录。 */
  missingModelBreakdown: boolean;
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

/**
 * Codex 额度接口返回的原始非敏感窗口元数据。
 * `usedPercent=null` 表示服务端尚未同步该字段，不能当成真实的 0%。
 */
export interface CodexQuotaWindow {
  usedPercent: number | null;
  windowSeconds: number | null;
  resetsAt: string | null;
}

export interface CodexQuotaSnapshot {
  quota: SubscriptionQuota;
  /** 即使 usedPercent 缺失，也保留周期长度与重置时间。 */
  quotaWindows?: CodexQuotaWindow[];
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
