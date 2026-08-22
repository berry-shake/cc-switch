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
