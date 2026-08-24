import type { CodexQuotaWindow, SubscriptionQuota } from "@/types/subscription";
import type { UsageSummary } from "@/types/usage";

const DAY_SECONDS = 24 * 60 * 60;

/**
 * Codex 的 primary window 通常只有 5 小时；容量外推只适用于周/月长周期。
 * 这里用接口返回的真实窗口长度判定，不依赖 tier 的展示名称。
 */
export const CODEX_LONG_CYCLE_MIN_SECONDS = 6 * DAY_SECONDS;

export type CodexQuotaCycleTier = SubscriptionQuota["tiers"][number];

export interface CodexQuotaCycleWindow {
  windowSeconds: number;
  /** null 表示服务端保留了周期，但尚未同步 used_percent。 */
  utilizationPercent: number | null;
  /** 本地同期汇总的查询起点。 */
  startMs: number;
  /** 本地同期汇总的查询终点，与官方 utilization 的采样时刻一致。 */
  endMs: number;
  /** 完整周期的服务端重置时刻，仅用于周期展示和有效性校验。 */
  resetAtMs: number;
}

export interface CodexQuotaCycle extends CodexQuotaCycleWindow {
  tier: CodexQuotaCycleTier;
  utilizationPercent: number;
  usedRatio: number;
}

export interface CodexCycleCapacityEstimate extends CodexQuotaCycle {
  remainingPercent: number;
  usedTokens: number;
  totalTokens: number;
  remainingTokens: number;
  usedUsd: number;
  totalUsd: number;
  remainingUsd: number;
}

export interface CodexCycleCapacityUsageBasis {
  usedTokens: number;
  usedUsd: number;
}

function isFinitePositive(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value > 0;
}

function isFiniteNonNegative(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

function parsePositiveCost(value: string): number | null {
  const parsed = Number(value);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : null;
}

/**
 * 从一次成功的 Codex 额度响应中解析当前长周期。
 *
 * 周期终点采用服务端 reset 时间；`queriedAt` 必须位于该周期内。这样既能
 * 排除过期缓存，也能让调用方先得到精确起止时间，再查询同口径的本地汇总。
 */
export function resolveCodexQuotaCycle(
  quota: SubscriptionQuota | null | undefined,
  nowMs: number = Date.now(),
): CodexQuotaCycle | null {
  if (
    !quota?.success ||
    !quota.tool.toLowerCase().startsWith("codex") ||
    !isFinitePositive(quota.queriedAt) ||
    !Number.isFinite(nowMs)
  ) {
    return null;
  }

  const queriedAtMs = quota.queriedAt;
  const candidates = quota.tiers
    .map((tier) => tier as CodexQuotaCycleTier)
    .filter((tier) => {
      const resetAtMs = tier.resetsAt ? Date.parse(tier.resetsAt) : NaN;
      return (
        isFinitePositive(tier.windowSeconds) &&
        tier.windowSeconds >= CODEX_LONG_CYCLE_MIN_SECONDS &&
        isFiniteNonNegative(tier.utilization) &&
        tier.utilization <= 100 &&
        Number.isFinite(resetAtMs) &&
        resetAtMs > queriedAtMs &&
        resetAtMs > nowMs &&
        resetAtMs - tier.windowSeconds * 1000 <= queriedAtMs
      );
    })
    .sort((a, b) => (b.windowSeconds ?? 0) - (a.windowSeconds ?? 0));

  const tier = candidates[0];
  if (!tier?.windowSeconds || !tier.resetsAt) return null;

  const cycleEndMs = Date.parse(tier.resetsAt);
  const usedRatio = tier.utilization / 100;

  return {
    tier,
    windowSeconds: tier.windowSeconds,
    utilizationPercent: tier.utilization,
    usedRatio,
    startMs: cycleEndMs - tier.windowSeconds * 1000,
    endMs: queriedAtMs,
    resetAtMs: cycleEndMs,
  };
}

/**
 * 解析可展示的当前长周期。
 *
 * 常规 `QuotaTier` 能区分真实 0%，而 Codex 专属原始窗口还能保留
 * `used_percent` 缺失的状态。这个函数只判断周期是否存在，不承诺当前已能
 * 进行容量外推。
 */
export function resolveCodexQuotaCycleWindow(
  quota: SubscriptionQuota | null | undefined,
  quotaWindows: readonly CodexQuotaWindow[] | null | undefined,
  nowMs: number = Date.now(),
): CodexQuotaCycleWindow | null {
  const cycle = resolveCodexQuotaCycle(quota, nowMs);
  if (cycle) return cycle;

  if (
    !quota?.success ||
    !quota.tool.toLowerCase().startsWith("codex") ||
    !isFinitePositive(quota.queriedAt) ||
    !Number.isFinite(nowMs)
  ) {
    return null;
  }

  const queriedAtMs = quota.queriedAt;
  const candidates = (quotaWindows ?? [])
    .filter((window) => {
      const resetAtMs = window.resetsAt ? Date.parse(window.resetsAt) : NaN;
      const utilization = window.usedPercent;
      return (
        isFinitePositive(window.windowSeconds) &&
        window.windowSeconds >= CODEX_LONG_CYCLE_MIN_SECONDS &&
        (utilization == null ||
          (isFiniteNonNegative(utilization) && utilization <= 100)) &&
        Number.isFinite(resetAtMs) &&
        resetAtMs > queriedAtMs &&
        resetAtMs > nowMs &&
        resetAtMs - window.windowSeconds * 1000 <= queriedAtMs
      );
    })
    .sort((a, b) => (b.windowSeconds ?? 0) - (a.windowSeconds ?? 0));

  const window = candidates[0];
  if (!window?.windowSeconds || !window.resetsAt) return null;

  const resetAtMs = Date.parse(window.resetsAt);
  return {
    windowSeconds: window.windowSeconds,
    utilizationPercent: window.usedPercent,
    startMs: resetAtMs - window.windowSeconds * 1000,
    endMs: queriedAtMs,
    resetAtMs,
  };
}

/** @deprecated 新调用方请使用语义更明确的 `resolveCodexQuotaCycle`。 */
export const resolveCodexLongCycle = resolveCodexQuotaCycle;

/**
 * 按官方已用比例，对同一周期内的本地 Codex Token 与 USD 等效成本作线性外推。
 * 结果是基于本周期模型、速度与 Token 结构的等效估算，不是官方承诺额度。
 */
export function estimateCodexCycleCapacity(
  cycle: CodexQuotaCycle | null | undefined,
  usage: UsageSummary | null | undefined,
): CodexCycleCapacityEstimate | null {
  if (!cycle || !usage) return null;

  const usedTokens = usage.realTotalTokens;
  const usedUsd = parsePositiveCost(usage.totalCost);
  if (usedUsd == null) return null;

  return estimateCodexCycleCapacityFromBasis(cycle, {
    usedTokens,
    usedUsd,
  });
}

/**
 * 对已经归一化的同期 Token/USD 基数进行容量外推。
 *
 * 本地 JSONL 与 Codex Web analytics 的数据来源不同，但最终都遵循
 * `同期实际值 ÷ 官方已用比例` 这一容量定义，因此在这里共享边界检查。
 */
export function estimateCodexCycleCapacityFromBasis(
  cycle: CodexQuotaCycle | null | undefined,
  basis: CodexCycleCapacityUsageBasis | null | undefined,
): CodexCycleCapacityEstimate | null {
  if (!cycle || !basis || !isFinitePositive(cycle.usedRatio)) return null;

  const { usedTokens, usedUsd } = basis;
  if (!isFinitePositive(usedTokens) || !isFinitePositive(usedUsd)) return null;

  const totalTokens = usedTokens / cycle.usedRatio;
  const totalUsd = usedUsd / cycle.usedRatio;
  const remainingTokens = Math.max(0, totalTokens - usedTokens);
  const remainingUsd = Math.max(0, totalUsd - usedUsd);

  if (
    !Number.isFinite(totalTokens) ||
    !Number.isFinite(totalUsd) ||
    !Number.isFinite(remainingTokens) ||
    !Number.isFinite(remainingUsd)
  ) {
    return null;
  }

  return {
    ...cycle,
    remainingPercent: Math.max(0, 100 - cycle.utilizationPercent),
    usedTokens,
    totalTokens,
    remainingTokens,
    usedUsd,
    totalUsd,
    remainingUsd,
  };
}

/** 便捷组合函数，适合纯展示组件与单元测试。 */
export function deriveCodexCycleCapacity(
  quota: SubscriptionQuota | null | undefined,
  usage: UsageSummary | null | undefined,
  nowMs: number = Date.now(),
): CodexCycleCapacityEstimate | null {
  return estimateCodexCycleCapacity(
    resolveCodexQuotaCycle(quota, nowMs),
    usage,
  );
}
