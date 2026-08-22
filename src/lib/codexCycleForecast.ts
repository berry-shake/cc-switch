const HOUR_MS = 60 * 60 * 1000;
const DAY_MS = 24 * HOUR_MS;

export const CODEX_RECENT_FORECAST_MIN_SPAN_MS = HOUR_MS;
export const CODEX_RECENT_FORECAST_MAX_SPAN_MS = DAY_MS;

const PERCENT_EPSILON = 1e-9;

export interface CodexUtilizationSample {
  timestampMs: number;
  utilizationPercent: number;
}

export interface CodexCycleForecastInput {
  cycleStartMs: number;
  resetAtMs: number;
  queriedAtMs: number;
  utilizationPercent: number;
  /**
   * 同一周期内的历史采样。调用方宜按时间升序传入；纯函数仍会排序、去重并
   * 丢弃周期外或时钟无效的数据，避免坏采样污染预测。
   */
  samples?: readonly CodexUtilizationSample[];
}

export type CodexForecastStatusLevel =
  | "below_pace"
  | "on_pace"
  | "above_pace"
  | "far_above_pace"
  | "exhausted";

export type CodexForecastRecentUnavailableReason =
  | "insufficient_samples"
  | "insufficient_span"
  | "utilization_regression";

export type CodexForecastExhaustion =
  | {
      kind: "at";
      atMs: number;
      withinCycle: boolean;
    }
  | {
      kind: "never";
      atMs: null;
      withinCycle: false;
    }
  | {
      kind: "unavailable";
      atMs: null;
      withinCycle: null;
    };

export interface CodexCycleForecast {
  elapsedMs: number;
  remainingMs: number;
  cycleTimeProgressRatio: number;
  cycleTimeProgressPercent: number;
  /** 按整个周期匀速使用时，当前时刻对应的线性基准用量。 */
  baselineUtilizationPercent: number;
  /** 当前实际用量 / 同期线性基准用量。 */
  paceRatio: number;
  currentUtilizationPercent: number;
  /** 整个周期均匀消耗 100% 时允许的平均日速率。 */
  sustainableRatePercentPerDay: number;
  cumulativeRatePercentPerDay: number;
  recentRatePercentPerDay: number | null;
  cumulativeExhaustion: CodexForecastExhaustion;
  recentExhaustion: CodexForecastExhaustion;
  /** 累计速率延续到周期结束时的原始用量预测，允许超过 100%。 */
  cumulativeProjectedUtilizationAtReset: number;
  /** 近期速率延续到周期结束时的原始用量预测，允许超过 100%。 */
  recentProjectedUtilizationAtReset: number | null;
  recentWindowStartMs: number | null;
  recentWindowEndMs: number | null;
  recentWindowSpanMs: number | null;
  recentUnavailableReason: CodexForecastRecentUnavailableReason | null;
  statusLevel: CodexForecastStatusLevel;
}

interface ForecastPoint {
  timestampMs: number;
  utilizationPercent: number;
}

interface RecentRateResult {
  ratePercentPerDay: number | null;
  windowStartMs: number | null;
  windowEndMs: number | null;
  windowSpanMs: number | null;
  unavailableReason: CodexForecastRecentUnavailableReason | null;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function clampPercent(value: number): number {
  return Math.min(100, Math.max(0, value));
}

function normalizeSamples(
  samples: readonly CodexUtilizationSample[],
  cycleStartMs: number,
  queriedAtMs: number,
): ForecastPoint[] {
  const sorted = samples
    .filter(
      (sample) =>
        isFiniteNumber(sample.timestampMs) &&
        sample.timestampMs >= cycleStartMs &&
        sample.timestampMs < queriedAtMs &&
        isFiniteNumber(sample.utilizationPercent) &&
        sample.utilizationPercent >= 0,
    )
    .map((sample) => ({
      timestampMs: sample.timestampMs,
      utilizationPercent: clampPercent(sample.utilizationPercent),
    }))
    .sort((a, b) => a.timestampMs - b.timestampMs);

  // 同一时刻多次采样时以后出现的值为准。时间跨度为 0 的重复点不能参与速率。
  const deduplicated: ForecastPoint[] = [];
  for (const point of sorted) {
    const last = deduplicated[deduplicated.length - 1];
    if (last?.timestampMs === point.timestampMs) {
      deduplicated[deduplicated.length - 1] = point;
    } else {
      deduplicated.push(point);
    }
  }
  return deduplicated;
}

function resolveRecentRate(
  history: readonly ForecastPoint[],
  current: ForecastPoint,
): RecentRateResult {
  if (history.length === 0) {
    return {
      ratePercentPerDay: null,
      windowStartMs: null,
      windowEndMs: null,
      windowSpanMs: null,
      unavailableReason: "insufficient_samples",
    };
  }

  const points = [...history, current];
  let monotonicStartIndex = 0;
  let hadRegression = false;

  // 额度比例下降通常代表账号/周期切换或服务端校正。近期速率不能跨越该边界。
  for (let index = 1; index < points.length; index += 1) {
    if (
      points[index].utilizationPercent + PERCENT_EPSILON <
      points[index - 1].utilizationPercent
    ) {
      monotonicStartIndex = index;
      hadRegression = true;
    }
  }

  const monotonicTail = points.slice(monotonicStartIndex);
  const recentLowerBound =
    current.timestampMs - CODEX_RECENT_FORECAST_MAX_SPAN_MS;
  const eligibleBaselines = monotonicTail.filter((point) => {
    const spanMs = current.timestampMs - point.timestampMs;
    return (
      point.timestampMs < current.timestampMs &&
      point.timestampMs >= recentLowerBound &&
      spanMs >= CODEX_RECENT_FORECAST_MIN_SPAN_MS
    );
  });

  // 选择近 24 小时内最早的合格点，获得最长但不超过 24h 的平滑观察窗。
  const baseline = eligibleBaselines[0];
  if (!baseline) {
    const tailHasPriorPoint = monotonicTail.some(
      (point) => point.timestampMs < current.timestampMs,
    );
    return {
      ratePercentPerDay: null,
      windowStartMs: null,
      windowEndMs: null,
      windowSpanMs: null,
      unavailableReason:
        hadRegression && !tailHasPriorPoint
          ? "utilization_regression"
          : tailHasPriorPoint
            ? "insufficient_span"
            : "insufficient_samples",
    };
  }

  const spanMs = current.timestampMs - baseline.timestampMs;
  const increment = current.utilizationPercent - baseline.utilizationPercent;
  if (!Number.isFinite(increment) || increment < -PERCENT_EPSILON) {
    return {
      ratePercentPerDay: null,
      windowStartMs: null,
      windowEndMs: null,
      windowSpanMs: null,
      unavailableReason: "utilization_regression",
    };
  }

  return {
    ratePercentPerDay: Math.max(0, increment) / (spanMs / DAY_MS),
    windowStartMs: baseline.timestampMs,
    windowEndMs: current.timestampMs,
    windowSpanMs: spanMs,
    unavailableReason: null,
  };
}

function exhaustionFromRate(
  currentUtilizationPercent: number,
  ratePercentPerDay: number | null,
  queriedAtMs: number,
  resetAtMs: number,
  unavailable = false,
): CodexForecastExhaustion {
  if (unavailable || ratePercentPerDay == null) {
    return { kind: "unavailable", atMs: null, withinCycle: null };
  }
  if (currentUtilizationPercent >= 100 - PERCENT_EPSILON) {
    return { kind: "at", atMs: queriedAtMs, withinCycle: true };
  }
  if (ratePercentPerDay <= PERCENT_EPSILON) {
    return { kind: "never", atMs: null, withinCycle: false };
  }

  const remainingPercent = 100 - currentUtilizationPercent;
  const atMs = queriedAtMs + (remainingPercent / ratePercentPerDay) * DAY_MS;
  if (!Number.isFinite(atMs)) {
    return { kind: "never", atMs: null, withinCycle: false };
  }
  return { kind: "at", atMs, withinCycle: atMs <= resetAtMs };
}

function projectedUtilizationAtReset(
  currentUtilizationPercent: number,
  ratePercentPerDay: number,
  remainingMs: number,
): number {
  return currentUtilizationPercent + ratePercentPerDay * (remainingMs / DAY_MS);
}

function statusFromPace(
  currentUtilizationPercent: number,
  paceRatio: number,
): CodexForecastStatusLevel {
  if (currentUtilizationPercent >= 100 - PERCENT_EPSILON) return "exhausted";
  if (paceRatio < 0.85) return "below_pace";
  if (paceRatio <= 1.15) return "on_pace";
  if (paceRatio <= 1.35) return "above_pace";
  return "far_above_pace";
}

/**
 * 根据一次当前额度和同周期历史采样计算消耗趋势。
 *
 * - 累计速率严格使用 `current utilization / 周期已过时间`；
 * - 近期速率使用最后一个单调段中、近 24h 内跨度至少 1h 的最长观察窗；
 * - 期末预测保留原始百分比（可超过 100%），便于显示真实超额趋势；
 * - 状态只比较当前实际用量与同期线性基准，不受近期预测波动影响；
 * - 主周期时钟无效时返回 `null`，单个坏历史点则被安全忽略。
 */
export function forecastCodexCycle(
  input: CodexCycleForecastInput,
): CodexCycleForecast | null {
  const { cycleStartMs, resetAtMs, queriedAtMs } = input;
  if (
    !isFiniteNumber(cycleStartMs) ||
    !isFiniteNumber(resetAtMs) ||
    !isFiniteNumber(queriedAtMs) ||
    !isFiniteNumber(input.utilizationPercent) ||
    input.utilizationPercent < 0 ||
    resetAtMs <= cycleStartMs ||
    queriedAtMs <= cycleStartMs ||
    queriedAtMs >= resetAtMs
  ) {
    return null;
  }

  const currentUtilizationPercent = clampPercent(input.utilizationPercent);
  const cycleDurationMs = resetAtMs - cycleStartMs;
  const elapsedMs = queriedAtMs - cycleStartMs;
  const remainingMs = resetAtMs - queriedAtMs;
  const cycleTimeProgressRatio = Math.min(
    1,
    Math.max(0, elapsedMs / cycleDurationMs),
  );
  const cycleTimeProgressPercent = cycleTimeProgressRatio * 100;
  const elapsedDays = elapsedMs / DAY_MS;
  const cycleDays = cycleDurationMs / DAY_MS;
  const sustainableRatePercentPerDay = 100 / cycleDays;
  const cumulativeRatePercentPerDay = currentUtilizationPercent / elapsedDays;
  const paceRatio = currentUtilizationPercent / cycleTimeProgressPercent;

  const history = normalizeSamples(
    input.samples ?? [],
    cycleStartMs,
    queriedAtMs,
  );
  const recent = resolveRecentRate(history, {
    timestampMs: queriedAtMs,
    utilizationPercent: currentUtilizationPercent,
  });

  const cumulativeProjection = projectedUtilizationAtReset(
    currentUtilizationPercent,
    cumulativeRatePercentPerDay,
    remainingMs,
  );
  const recentProjection =
    recent.ratePercentPerDay == null
      ? null
      : projectedUtilizationAtReset(
          currentUtilizationPercent,
          recent.ratePercentPerDay,
          remainingMs,
        );

  return {
    elapsedMs,
    remainingMs,
    cycleTimeProgressRatio,
    cycleTimeProgressPercent,
    baselineUtilizationPercent: cycleTimeProgressPercent,
    paceRatio,
    currentUtilizationPercent,
    sustainableRatePercentPerDay,
    cumulativeRatePercentPerDay,
    recentRatePercentPerDay: recent.ratePercentPerDay,
    cumulativeExhaustion: exhaustionFromRate(
      currentUtilizationPercent,
      cumulativeRatePercentPerDay,
      queriedAtMs,
      resetAtMs,
    ),
    recentExhaustion: exhaustionFromRate(
      currentUtilizationPercent,
      recent.ratePercentPerDay,
      queriedAtMs,
      resetAtMs,
      recent.unavailableReason != null,
    ),
    cumulativeProjectedUtilizationAtReset: cumulativeProjection,
    recentProjectedUtilizationAtReset: recentProjection,
    recentWindowStartMs: recent.windowStartMs,
    recentWindowEndMs: recent.windowEndMs,
    recentWindowSpanMs: recent.windowSpanMs,
    recentUnavailableReason: recent.unavailableReason,
    statusLevel: statusFromPace(currentUtilizationPercent, paceRatio),
  };
}
