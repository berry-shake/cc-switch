import { invoke } from "@tauri-apps/api/core";
import { resolveCodexQuotaCycle } from "@/lib/codexCycleCapacity";
import {
  recordCodexQuotaSample,
  sampleCodexQuotaCycle,
} from "@/lib/codexQuotaSamples";
import type {
  CodexAnalyticsUsage,
  SubscriptionQuota,
} from "@/types/subscription";

export const subscriptionApi = {
  getQuota: async (tool: string): Promise<SubscriptionQuota> => {
    const quota = await invoke<SubscriptionQuota>("get_subscription_quota", {
      tool,
    });

    // 统一记录所有前端入口取得的 Codex CLI 长周期快照。记录失败只会让
    // 近期趋势暂不可用，不能改变原额度请求的成功语义。
    if (tool.trim().toLowerCase() === "codex") {
      const cycle = resolveCodexQuotaCycle(quota);
      if (cycle) recordCodexQuotaSample(sampleCodexQuotaCycle(cycle));
    }

    return quota;
  },
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
