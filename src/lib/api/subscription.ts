import { invoke } from "@tauri-apps/api/core";
import { resolveCodexQuotaCycle } from "@/lib/codexCycleCapacity";
import {
  recordCodexQuotaSample,
  sampleCodexQuotaCycle,
} from "@/lib/codexQuotaSamples";
import type {
  CodexAnalyticsUsage,
  CodexOfficialUsageSnapshot,
  CodexQuotaSnapshot,
  SubscriptionQuota,
} from "@/types/subscription";

function recordCodexSnapshotSample<T extends CodexQuotaSnapshot>(
  snapshot: T,
): T {
  const cycle = resolveCodexQuotaCycle(snapshot.quota);
  if (cycle) {
    recordCodexQuotaSample(
      sampleCodexQuotaCycle(cycle, snapshot.credentialScope),
    );
  }
  return snapshot;
}

export const subscriptionApi = {
  getQuota: (tool: string): Promise<SubscriptionQuota> =>
    invoke<SubscriptionQuota>("get_subscription_quota", {
      tool,
    }),
  getCodexQuotaSnapshot: async (): Promise<CodexQuotaSnapshot> =>
    recordCodexSnapshotSample(
      await invoke<CodexQuotaSnapshot>("get_codex_quota_snapshot"),
    ),
  getCodexOfficialUsageSnapshot:
    async (): Promise<CodexOfficialUsageSnapshot> =>
      recordCodexSnapshotSample(
        await invoke<CodexOfficialUsageSnapshot>(
          "get_codex_official_usage_snapshot",
        ),
      ),
  getCodexOauthQuota: (accountId: string | null): Promise<SubscriptionQuota> =>
    invoke("get_codex_oauth_quota", { accountId }),
  getCodexUsageAnalytics: (
    startDate: string,
    endDate: string,
  ): Promise<CodexAnalyticsUsage> =>
    invoke("get_codex_usage_analytics", { startDate, endDate }),
  getXaiOauthQuota: (accountId: string | null): Promise<SubscriptionQuota> =>
    invoke("get_xai_oauth_quota", { accountId }),
  getCodingPlanQuota: (
    baseUrl: string,
    apiKey: string,
    // 火山方舟用账号 AK/SK 签名查询用量；其他供应商不传。
    accessKeyId?: string,
    secretAccessKey?: string,
    // 智谱团队版（zhipu_team）靠显式标识路由（base_url 与个人版相同无法区分）。
    codingPlanProvider?: string,
    teamOrganizationId?: string,
    teamProjectId?: string,
  ): Promise<SubscriptionQuota> =>
    invoke("get_coding_plan_quota", {
      baseUrl,
      apiKey,
      accessKeyId,
      secretAccessKey,
      codingPlanProvider,
      teamOrganizationId,
      teamProjectId,
    }),
  getBalance: (
    baseUrl: string,
    apiKey: string,
  ): Promise<import("@/types").UsageResult> =>
    invoke("get_balance", { baseUrl, apiKey }),
};
