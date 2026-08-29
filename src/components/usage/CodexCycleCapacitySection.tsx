import { useCallback, useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { subscriptionApi } from "@/lib/api/subscription";
import { usageApi } from "@/lib/api/usage";
import { resolveCodexQuotaCycle } from "@/lib/codexCycleCapacity";
import {
  readCodexCycleCapacityMode,
  type CodexCycleCapacityCalculationMode,
} from "@/lib/codexCycleCapacityMode";
import { loadCodexQuotaSamples } from "@/lib/codexQuotaSamples";
import { subscriptionKeys } from "@/lib/query/subscription";
import { usageKeys } from "@/lib/query/usage";

import { CodexCycleCapacityCard } from "./CodexCycleCapacityCard";

// 本地与官方两种模式打的是同一个 ChatGPT 额度端点，共用同一刷新节奏，
// 避免本地模式以更高频率重复消耗官方接口。
const QUOTA_REFRESH_INTERVAL_MS = 60 * 60 * 1000;
const DEFAULT_USAGE_REFRESH_INTERVAL_MS = 30 * 1000;

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
 * 最新请求 reject 或 HTTP 错误映射为 success=false 时，入口保持隐藏；接口
 * 成功但周期刚重置、比例尚未同步或用量暂为空时，则保留卡片并显示等待状态。
 */
export function CodexCycleCapacitySection({
  enabled,
  refreshIntervalMs = DEFAULT_USAGE_REFRESH_INTERVAL_MS,
}: CodexCycleCapacitySectionProps) {
  const autoRefresh = refreshIntervalMs > 0;
  const [calculationMode, setCalculationMode] = useState(
    readCodexCycleCapacityMode,
  );
  const localQuotaQuery = useQuery({
    queryKey: subscriptionKeys.codexQuotaSnapshot(),
    queryFn: subscriptionApi.getCodexQuotaSnapshot,
    enabled: enabled && calculationMode === "local",
    retry: 1,
    staleTime: QUOTA_REFRESH_INTERVAL_MS,
    refetchInterval:
      autoRefresh && calculationMode === "local"
        ? QUOTA_REFRESH_INTERVAL_MS
        : false,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: autoRefresh,
  });

  const officialQuery = useQuery({
    queryKey: subscriptionKeys.codexOfficialSnapshot(),
    queryFn: subscriptionApi.getCodexOfficialUsageSnapshot,
    // 本地模式也预取一次，用来判断官方模式是否可用；只在选中官方模式时轮询。
    enabled,
    retry: 1,
    staleTime: QUOTA_REFRESH_INTERVAL_MS,
    refetchInterval:
      autoRefresh && calculationMode === "analytics"
        ? QUOTA_REFRESH_INTERVAL_MS
        : false,
    refetchIntervalInBackground: false,
    refetchOnWindowFocus: false,
  });

  // react-query 会在后台重取失败时保留旧 data；必须同时要求 isSuccess，
  // 才不会在接口已经失败后继续拿陈旧快照显示容量入口。
  const activeSnapshot =
    calculationMode === "analytics"
      ? officialQuery.isSuccess
        ? officialQuery.data
        : null
      : localQuotaQuery.isSuccess
        ? localQuotaQuery.data
        : null;
  const quota = activeSnapshot?.quota ?? null;
  const cycle = useMemo(() => resolveCodexQuotaCycle(quota), [quota]);
  const startDate = cycle ? Math.floor(cycle.startMs / 1000) : undefined;
  const endDate = cycle ? Math.floor(cycle.endMs / 1000) : undefined;
  const quotaSamples = useMemo(
    () =>
      cycle && activeSnapshot
        ? loadCodexQuotaSamples(activeSnapshot.credentialScope)
        : [],
    [activeSnapshot, cycle],
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

  const refreshAnalytics = useCallback(async () => {
    await Promise.all([officialQuery.refetch(), pricingQuery.refetch()]);
  }, [officialQuery, pricingQuery]);

  const handleCalculationModeChange = useCallback(
    (mode: CodexCycleCapacityCalculationMode) => {
      setCalculationMode(mode);
      if (
        mode === "analytics" &&
        // 同时覆盖一小时自然过期和账号切换触发的显式失效。
        officialQuery.isStale &&
        !officialQuery.isFetching
      ) {
        void refreshAnalytics();
      }
    },
    [officialQuery.isFetching, officialQuery.isStale, refreshAnalytics],
  );

  const analyticsUsage =
    officialQuery.isSuccess &&
    officialQuery.data.credentialScope === activeSnapshot?.credentialScope
      ? officialQuery.data.analytics
      : null;
  if (!enabled || !activeSnapshot || !quota?.success) return null;

  return (
    <CodexCycleCapacityCard
      quota={quota}
      quotaWindows={activeSnapshot.quotaWindows}
      accountEmail={activeSnapshot?.email}
      lastRefreshedAt={activeSnapshot?.quota.queriedAt}
      usage={usageQuery.isSuccess ? usageQuery.data : null}
      analyticsUsage={analyticsUsage}
      modelPricing={pricingQuery.isSuccess ? pricingQuery.data : null}
      quotaSamples={quotaSamples}
      calculationMode={calculationMode}
      onCalculationModeChange={handleCalculationModeChange}
      onRefreshAnalytics={refreshAnalytics}
      isRefreshingAnalytics={Boolean(
        officialQuery.isFetching || pricingQuery.isFetching,
      )}
    />
  );
}
