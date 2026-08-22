import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import { subscriptionApi } from "@/lib/api/subscription";
import { usageApi } from "@/lib/api/usage";
import { resolveCodexQuotaCycle } from "@/lib/codexCycleCapacity";
import { loadCodexQuotaSamples } from "@/lib/codexQuotaSamples";
import { subscriptionKeys } from "@/lib/query/subscription";
import { usageKeys } from "@/lib/query/usage";

import { CodexCycleCapacityCard } from "./CodexCycleCapacityCard";

const QUOTA_REFRESH_INTERVAL_MS = 5 * 60 * 1000;
const ANALYTICS_REFRESH_INTERVAL_MS = 5 * 60 * 1000;
const DEFAULT_USAGE_REFRESH_INTERVAL_MS = 30 * 1000;
const DAY_MS = 24 * 60 * 60 * 1000;

function utcDateKey(timestampMs: number): string {
  return new Date(timestampMs).toISOString().slice(0, 10);
}

export interface CodexCycleCapacitySectionProps {
  /** 入口只在「全部」或 Codex 统计上下文中启用。 */
  enabled: boolean;
  refreshIntervalMs?: number;
}

/**
 * Codex 周期容量的数据容器。
 *
 * 本机 JSONL 用量属于当前 Codex CLI 账号，因此额度也只从同一份
 * `~/.codex/auth.json` 凭据查询，不与 cc-switch 托管的其他 OAuth 账号混算。
 * 这里故意直接读取原始 react-query 状态，不使用订阅卡片的 keep-last-good：
 * 最新请求 reject、HTTP 错误映射为 success=false，或返回缺失长周期数据时，
 * 整个入口都会消失。
 */
export function CodexCycleCapacitySection({
  enabled,
  refreshIntervalMs = DEFAULT_USAGE_REFRESH_INTERVAL_MS,
}: CodexCycleCapacitySectionProps) {
  const autoRefresh = refreshIntervalMs > 0;
  const quotaQuery = useQuery({
    queryKey: subscriptionKeys.quota("codex"),
    queryFn: () => subscriptionApi.getQuota("codex"),
    enabled,
    retry: 1,
    staleTime: 60_000,
    refetchInterval: autoRefresh ? QUOTA_REFRESH_INTERVAL_MS : false,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: autoRefresh,
  });

  // react-query 会在后台重取失败时保留旧 data；必须同时要求 isSuccess，
  // 才不会在接口已经失败后继续拿陈旧快照显示容量入口。
  const quota =
    quotaQuery.isSuccess && quotaQuery.data.success ? quotaQuery.data : null;
  const cycle = useMemo(() => resolveCodexQuotaCycle(quota), [quota]);
  const startDate = cycle ? Math.floor(cycle.startMs / 1000) : undefined;
  const endDate = cycle ? Math.floor(cycle.endMs / 1000) : undefined;
  // usage.js 会把查询边界各放宽一天，再在前端按 UTC 日桶筛回当前周期。
  // 这样能覆盖周期在某天中途开始或接口日桶延迟同步的情况。
  const analyticsStartDate = cycle
    ? utcDateKey(cycle.startMs - DAY_MS)
    : undefined;
  const analyticsEndDate = cycle ? utcDateKey(cycle.endMs + DAY_MS) : undefined;
  const quotaSamples = useMemo(
    () => (cycle ? loadCodexQuotaSamples() : []),
    [cycle],
  );

  const usageQuery = useQuery({
    queryKey: usageKeys.summary(
      "custom",
      startDate,
      endDate,
      { appType: "codex" },
      false,
    ),
    queryFn: () =>
      usageApi.getUsageSummary(
        startDate,
        endDate,
        "codex",
        undefined,
        undefined,
      ),
    enabled: enabled && cycle != null,
    refetchInterval: autoRefresh ? refreshIntervalMs : false,
    refetchIntervalInBackground: false,
  });

  const pricingQuery = useQuery({
    queryKey: usageKeys.pricing(),
    queryFn: usageApi.getModelPricing,
    enabled: enabled && cycle != null,
    staleTime: 5 * 60 * 1000,
  });

  const analyticsQuery = useQuery({
    queryKey:
      analyticsStartDate && analyticsEndDate
        ? subscriptionKeys.codexAnalytics(analyticsStartDate, analyticsEndDate)
        : subscriptionKeys.codexAnalytics("pending", "pending"),
    queryFn: () =>
      subscriptionApi.getCodexUsageAnalytics(
        analyticsStartDate!,
        analyticsEndDate!,
      ),
    enabled:
      enabled &&
      cycle != null &&
      analyticsStartDate != null &&
      analyticsEndDate != null,
    retry: 1,
    staleTime: 60_000,
    refetchInterval: autoRefresh ? ANALYTICS_REFRESH_INTERVAL_MS : false,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: autoRefresh,
  });

  const hasLocalUsage = usageQuery.isSuccess;
  const hasAnalyticsUsage = analyticsQuery.isSuccess && pricingQuery.isSuccess;
  if (!enabled || !cycle || (!hasLocalUsage && !hasAnalyticsUsage)) return null;

  return (
    <CodexCycleCapacityCard
      quota={quota}
      usage={usageQuery.isSuccess ? usageQuery.data : null}
      analyticsUsage={analyticsQuery.isSuccess ? analyticsQuery.data : null}
      modelPricing={pricingQuery.isSuccess ? pricingQuery.data : null}
      quotaSamples={quotaSamples}
    />
  );
}
