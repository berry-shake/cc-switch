import {
  estimateCodexCycleCapacityFromBasis,
  resolveCodexQuotaCycle,
  type CodexCycleCapacityEstimate,
  type CodexQuotaCycle,
  type CodexQuotaCycleWindow,
} from "@/lib/codexCycleCapacity";
import type {
  CodexAnalyticsAccountMode,
  CodexAnalyticsDailyUsage,
  CodexAnalyticsModelUsage,
  CodexAnalyticsTokenCounts,
  CodexAnalyticsUsage,
  SubscriptionQuota,
} from "@/types/subscription";
import type { ModelPricing } from "@/types/usage";

const EPSILON = 1e-9;
const DAY_MS = 24 * 60 * 60 * 1000;

interface ModelPrice {
  uncachedInput: number;
  cachedInput: number;
  cacheWriteInput: number;
  output: number;
}

type PricingLookup = ReadonlyMap<string, ModelPrice>;

export interface CodexAnalyticsUsageBasis {
  accountMode: CodexAnalyticsAccountMode;
  usedTokens: number;
  usedUsd: number;
  hasUnknownPricing: boolean;
  hasEstimatedAllocation: boolean;
  includedDays: number;
}

export interface CodexAnalyticsCycleCapacityEstimate
  extends CodexCycleCapacityEstimate,
    CodexAnalyticsUsageBasis {}

export interface CodexAnalyticsCycleDataQuality {
  /** 已有模型额度明细、但总 Token 日报尚未同步的 UTC 日期。 */
  missingTokenDates: string[];
  /** 已有总 Token 日报、但模型/速度额度明细尚未同步的 UTC 日期。 */
  missingModelBreakdownDates: string[];
  /** 周期从该 UTC 日期中途开始，且误差不可忽略时才给出；否则为 null。 */
  partialStartDate: string | null;
  /** 起始日可能混入的重置前用量占已统计 Token 的比例；无起始日误差时为 0。 */
  partialStartOverstatementRatio: number;
}

interface TokenParts {
  uncachedInput: number;
  cachedInput: number;
  cacheWriteInput: number;
  output: number;
}

interface ModelWeight {
  item: CodexAnalyticsModelUsage;
  weight: number;
}

function finiteNonNegative(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) && value > 0
    ? value
    : 0;
}

function clamp(value: number, min = 0, max = 1): number {
  return Math.min(max, Math.max(min, value));
}

function normalizeModel(value: string): string {
  return (value || "unknown").trim().toLowerCase() || "unknown";
}

function normalizeSpeed(value: string): string {
  return (value || "standard").trim().toLowerCase() || "standard";
}

function parsePrice(value: string): number | null {
  const parsed = Number(value);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : null;
}

function buildPricingLookup(pricing: readonly ModelPricing[]): PricingLookup {
  const lookup = new Map<string, ModelPrice>();
  for (const entry of pricing) {
    const uncachedInput = parsePrice(entry.inputCostPerMillion);
    const cachedInput = parsePrice(entry.cacheReadCostPerMillion);
    const cacheWriteInput = parsePrice(entry.cacheCreationCostPerMillion);
    const output = parsePrice(entry.outputCostPerMillion);
    if (
      uncachedInput == null ||
      cachedInput == null ||
      cacheWriteInput == null ||
      output == null
    ) {
      continue;
    }
    lookup.set(normalizeModel(entry.modelId), {
      uncachedInput,
      cachedInput,
      cacheWriteInput,
      output,
    });
  }
  return lookup;
}

function modelFamily(modelName: string): string {
  const normalized = normalizeModel(modelName);
  for (const suffix of ["-minimal", "-low", "-medium", "-high", "-xhigh"]) {
    if (normalized.endsWith(suffix)) {
      return normalized.slice(0, -suffix.length);
    }
  }
  return normalized;
}

/** 与 usage.js 及本地 Codex JSONL 导入保持一致的额度倍数。 */
function fastMultiplier(modelName: string, speed: string): number {
  if (normalizeSpeed(speed) !== "fast") return 1;
  const family = modelFamily(modelName);
  if (
    [
      "gpt-6-astra",
      "gpt-5.6",
      "gpt-5.6-sol",
      "gpt-5.6-terra",
      "gpt-5.6-luna",
      "gpt-5.5",
    ].includes(family)
  ) {
    return 2.5;
  }
  return family === "gpt-5.4" || family === "gpt-5.4-mini" ? 2 : 1;
}

function tokenParts(tokens: CodexAnalyticsTokenCounts): TokenParts {
  return {
    uncachedInput: finiteNonNegative(tokens.uncachedInputTokens),
    cachedInput: finiteNonNegative(tokens.cachedInputTokens),
    cacheWriteInput: finiteNonNegative(tokens.cacheWriteInputTokens),
    output: finiteNonNegative(tokens.outputTokens),
  };
}

function tokenTotal(tokens: CodexAnalyticsTokenCounts): number {
  const reported = finiteNonNegative(tokens.totalTokens);
  if (reported > 0) return reported;
  const parts = tokenParts(tokens);
  return (
    parts.uncachedInput +
    parts.cachedInput +
    parts.cacheWriteInput +
    parts.output
  );
}

function getModelPrice(
  modelName: string,
  speed: string,
  pricing: PricingLookup,
): ModelPrice | null {
  const normalized = normalizeModel(modelName);
  const base = pricing.get(normalized) ?? pricing.get(modelFamily(normalized));
  if (!base) return null;
  const multiplier = fastMultiplier(modelName, speed);
  return {
    uncachedInput: base.uncachedInput * multiplier,
    cachedInput: base.cachedInput * multiplier,
    cacheWriteInput: base.cacheWriteInput * multiplier,
    output: base.output * multiplier,
  };
}

function estimateModelUsd(
  model: Pick<CodexAnalyticsModelUsage, "model" | "speed" | "tokens">,
  pricing: PricingLookup,
): number | null {
  const price = getModelPrice(model.model, model.speed, pricing);
  if (!price) return null;
  const parts = tokenParts(model.tokens);
  return (
    (parts.uncachedInput / 1_000_000) * price.uncachedInput +
    (parts.cachedInput / 1_000_000) * price.cachedInput +
    (parts.cacheWriteInput / 1_000_000) * price.cacheWriteInput +
    (parts.output / 1_000_000) * price.output
  );
}

function allocateIntegerTotal(
  total: number,
  shares: readonly number[],
): number[] {
  const normalizedTotal = Math.max(0, Math.round(finiteNonNegative(total)));
  if (shares.length === 0) return [];

  const raw = shares.map((share) => normalizedTotal * clamp(share));
  const allocated = raw.map(Math.floor);
  let remainder =
    normalizedTotal - allocated.reduce((sum, value) => sum + value, 0);
  const order = raw
    .map((value, index) => ({
      index,
      fraction: value - Math.floor(value),
    }))
    .sort((a, b) => b.fraction - a.fraction || a.index - b.index);

  for (let index = 0; remainder > 0; index += 1, remainder -= 1) {
    allocated[order[index % order.length].index] += 1;
  }
  return allocated;
}

function getRateAdjustedShares(
  modelWeights: readonly ModelWeight[],
  totals: CodexAnalyticsTokenCounts,
  pricing: PricingLookup,
): { shares: number[]; hasRateFallback: boolean } {
  const creditWeightTotal = modelWeights.reduce(
    (sum, entry) => sum + entry.weight,
    0,
  );
  const creditShares = modelWeights.map(
    (entry) => entry.weight / creditWeightTotal,
  );
  if (modelWeights.length === 1) {
    return { shares: [1], hasRateFallback: false };
  }

  const parts = tokenParts(totals);
  const classifiedTokens =
    parts.uncachedInput +
    parts.cachedInput +
    parts.cacheWriteInput +
    parts.output;
  if (classifiedTokens <= EPSILON) {
    return { shares: creditShares, hasRateFallback: true };
  }

  const rates = modelWeights.map(({ item }) => {
    const price = getModelPrice(item.model, item.speed, pricing);
    if (!price) return null;
    const rate =
      (parts.uncachedInput / classifiedTokens) * price.uncachedInput +
      (parts.cachedInput / classifiedTokens) * price.cachedInput +
      (parts.cacheWriteInput / classifiedTokens) * price.cacheWriteInput +
      (parts.output / classifiedTokens) * price.output;
    return rate > EPSILON ? rate : null;
  });
  const known = rates
    .map((rate, index) => ({ rate, index }))
    .filter((entry): entry is { rate: number; index: number } =>
      Number.isFinite(entry.rate),
    );
  if (known.length === 0) {
    return { shares: creditShares, hasRateFallback: true };
  }

  const knownWeight = known.reduce(
    (sum, { index }) => sum + modelWeights[index].weight,
    0,
  );
  const fallbackRate = known.reduce(
    (sum, { rate, index }) =>
      sum + rate * (modelWeights[index].weight / knownWeight),
    0,
  );
  const inverseRateWeights = modelWeights.map(
    (entry, index) => entry.weight / (rates[index] ?? fallbackRate),
  );
  const inverseRateWeightTotal = inverseRateWeights.reduce(
    (sum, value) => sum + value,
    0,
  );
  return {
    shares: inverseRateWeights.map((value) => value / inverseRateWeightTotal),
    hasRateFallback: known.length !== modelWeights.length,
  };
}

function allocatePersonalModels(
  day: CodexAnalyticsDailyUsage,
  pricing: PricingLookup,
): {
  models: CodexAnalyticsModelUsage[];
  hasEstimatedAllocation: boolean;
} {
  let modelWeights: ModelWeight[] = day.models
    .map((item) => ({ item, weight: finiteNonNegative(item.credits) }))
    .filter(({ weight }) => weight > EPSILON);
  const hasBreakdown = modelWeights.length > 0;
  if (!hasBreakdown) {
    modelWeights = [
      {
        item: {
          model: "unknown",
          speed: "standard",
          credits: 1,
          tokens: emptyTokenCounts(),
        },
        weight: 1,
      },
    ];
  }

  const allocation = getRateAdjustedShares(modelWeights, day.totals, pricing);
  const parts = tokenParts(day.totals);
  const uncached = allocateIntegerTotal(parts.uncachedInput, allocation.shares);
  const cached = allocateIntegerTotal(parts.cachedInput, allocation.shares);
  const cacheWrite = allocateIntegerTotal(
    parts.cacheWriteInput,
    allocation.shares,
  );
  const output = allocateIntegerTotal(parts.output, allocation.shares);

  return {
    models: modelWeights.map(({ item }, index) => ({
      ...item,
      tokens: {
        uncachedInputTokens: uncached[index],
        cachedInputTokens: cached[index],
        cacheWriteInputTokens: cacheWrite[index],
        outputTokens: output[index],
        totalTokens:
          uncached[index] + cached[index] + cacheWrite[index] + output[index],
      },
    })),
    hasEstimatedAllocation:
      modelWeights.length > 1 || !hasBreakdown || allocation.hasRateFallback,
  };
}

function emptyTokenCounts(): CodexAnalyticsTokenCounts {
  return {
    uncachedInputTokens: 0,
    cachedInputTokens: 0,
    cacheWriteInputTokens: 0,
    outputTokens: 0,
    totalTokens: 0,
  };
}

function utcDateKey(timestampMs: number): string {
  return new Date(timestampMs).toISOString().slice(0, 10);
}

function currentCycleDays(
  days: readonly CodexAnalyticsDailyUsage[],
  cycle: Pick<CodexQuotaCycleWindow, "startMs" | "endMs" | "resetAtMs">,
): CodexAnalyticsDailyUsage[] {
  const startDate = utcDateKey(cycle.startMs);
  const endDate = utcDateKey(Math.min(cycle.endMs, cycle.resetAtMs));
  return days.filter((day) => {
    const date = day.date.slice(0, 10);
    return date >= startDate && date <= endDate;
  });
}

/**
 * 起始日误差低于该比例时不提示：面板底部的免责声明已经覆盖了这一类系统性
 * 偏差，只有真正会影响读数的边界日才值得单独占一条告警。
 */
const PARTIAL_START_MIN_OVERSTATEMENT_RATIO = 0.05;

/**
 * 估算起始日可能混入的重置前用量占已统计 Token 的比例。
 *
 * 接口只给到日粒度，无法知道当天用量落在重置前还是重置后；这里按当天用量在
 * 日内均匀分布估算，取「重置时刻之前那部分时长」的占比作为期望误差。
 */
function partialStartOverstatement(
  days: readonly CodexAnalyticsDailyUsage[],
  cycleStartMs: number,
): number {
  const preStartFraction = (cycleStartMs % DAY_MS) / DAY_MS;
  if (preStartFraction <= 0) return 0;

  const startDate = utcDateKey(cycleStartMs);
  let startDayTokens = 0;
  let includedTokens = 0;
  for (const day of days) {
    const dayTokens = tokenTotal(day.totals);
    includedTokens += dayTokens;
    if (day.date.slice(0, 10) === startDate) startDayTokens += dayTokens;
  }
  if (includedTokens <= EPSILON) return 0;
  return (startDayTokens * preStartFraction) / includedTokens;
}

/**
 * 检查当前官方日统计是否完整，并标记无法按精确时刻切分的周期起始日。
 * 这里沿用容量计算的 UTC 日期口径，避免提示与实际纳入的日桶不一致；
 * 调用方应传入与容量外推同一个周期，否则提示会描述另一段时间。
 */
export function inspectCodexAnalyticsCycleData(
  cycle:
    | Pick<CodexQuotaCycleWindow, "startMs" | "endMs" | "resetAtMs">
    | null
    | undefined,
  analytics: CodexAnalyticsUsage | null | undefined,
): CodexAnalyticsCycleDataQuality | null {
  if (!cycle || !analytics) return null;

  const days = currentCycleDays(analytics.days, cycle);
  const overstatementRatio = partialStartOverstatement(days, cycle.startMs);
  const isMaterial =
    overstatementRatio >= PARTIAL_START_MIN_OVERSTATEMENT_RATIO;

  return {
    missingTokenDates: days
      .filter((day) => day.missingTokenData)
      .map((day) => day.date.slice(0, 10)),
    missingModelBreakdownDates: days
      .filter((day) => day.missingModelBreakdown)
      .map((day) => day.date.slice(0, 10)),
    partialStartDate: isMaterial ? utcDateKey(cycle.startMs) : null,
    partialStartOverstatementRatio: isMaterial ? overstatementRatio : 0,
  };
}

export function buildCodexAnalyticsUsageBasis(
  cycle: CodexQuotaCycle | null | undefined,
  analytics: CodexAnalyticsUsage | null | undefined,
  modelPricing: readonly ModelPricing[] | null | undefined,
): CodexAnalyticsUsageBasis | null {
  // Personal raw Credits have an independent, nullable-data path. Never fall
  // back to price-derived dollars when that path is incomplete or unavailable.
  if (analytics?.accountMode === "personal" && analytics.personalCredits)
    return null;
  if (!cycle || !analytics || !modelPricing) return null;
  const pricing = buildPricingLookup(modelPricing);
  if (pricing.size === 0) return null;
  const days = currentCycleDays(analytics.days, cycle);
  if (days.length === 0) return null;

  let usedTokens = 0;
  let usedUsd = 0;
  let hasUnknownPricing = false;
  let hasEstimatedAllocation = false;

  for (const day of days) {
    usedTokens += tokenTotal(day.totals);
    const allocated =
      analytics.accountMode === "personal"
        ? allocatePersonalModels(day, pricing)
        : { models: day.models, hasEstimatedAllocation: false };
    hasEstimatedAllocation ||= allocated.hasEstimatedAllocation;
    for (const model of allocated.models) {
      if (tokenTotal(model.tokens) <= EPSILON) continue;
      const modelUsd = estimateModelUsd(model, pricing);
      if (modelUsd == null) {
        hasUnknownPricing = true;
      } else {
        usedUsd += modelUsd;
      }
    }
  }

  if (usedTokens <= EPSILON || usedUsd <= EPSILON) return null;
  return {
    accountMode: analytics.accountMode,
    usedTokens,
    usedUsd,
    hasUnknownPricing,
    hasEstimatedAllocation,
    includedDays: days.length,
  };
}

export function deriveCodexAnalyticsCycleCapacity(
  quota: SubscriptionQuota | null | undefined,
  analytics: CodexAnalyticsUsage | null | undefined,
  modelPricing: readonly ModelPricing[] | null | undefined,
  nowMs: number = Date.now(),
): CodexAnalyticsCycleCapacityEstimate | null {
  const cycle = resolveCodexQuotaCycle(quota, nowMs);
  const basis = buildCodexAnalyticsUsageBasis(cycle, analytics, modelPricing);
  const estimate = estimateCodexCycleCapacityFromBasis(cycle, basis);
  return estimate && basis ? { ...estimate, ...basis } : null;
}
