import type { TFunction } from "i18next";
import { Activity } from "lucide-react";

import type {
  CodexCycleForecast,
  CodexForecastExhaustion,
  CodexForecastStatusLevel,
} from "@/lib/codexCycleForecast";
import { cn } from "@/lib/utils";

const EXHAUSTION_TIME_EPSILON_MS = 1_000;

function formatPercent(value: number): string {
  const rounded = Math.round(value);
  return Math.abs(value - rounded) < 0.05
    ? `${rounded}%`
    : `${value.toFixed(1)}%`;
}

function formatDateTime(timestamp: number, locale: string): string {
  return new Intl.DateTimeFormat(locale, {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function formatDuration(
  durationMs: number,
  lang: string,
  units: { day: string; hour: string; minute: string },
): string {
  const totalMinutes = Math.max(0, Math.floor(durationMs / 60_000));
  const days = Math.floor(totalMinutes / (24 * 60));
  const hours = Math.floor((totalMinutes % (24 * 60)) / 60);
  const minutes = totalMinutes % 60;
  const parts: string[] = [];

  if (days > 0) parts.push(`${days}${units.day}`);
  if (hours > 0) parts.push(`${hours}${units.hour}`);
  if (minutes > 0 || parts.length === 0)
    parts.push(`${minutes}${units.minute}`);

  return parts.join(lang.startsWith("zh") || lang.startsWith("ja") ? "" : " ");
}

function formatRate(value: number, perDay: string): string {
  return `${value.toFixed(1)}${perDay}`;
}

function ForecastRow({
  label,
  value,
  valueClassName,
}: {
  label: string;
  value: string;
  valueClassName?: string;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-1 border-b border-border/40 py-2.5 last:border-b-0 sm:flex-row sm:items-baseline sm:justify-between sm:gap-5">
      <dt className="text-xs font-medium leading-relaxed text-muted-foreground">
        {label}
      </dt>
      <dd
        className={cn(
          "text-sm font-semibold leading-relaxed tabular-nums text-foreground sm:text-right",
          valueClassName,
        )}
      >
        {value}
      </dd>
    </div>
  );
}

function statusPresentation(
  status: CodexForecastStatusLevel,
  t: TFunction,
): { label: string; comparison: string; className: string } {
  switch (status) {
    case "below_pace":
      return {
        label: t("usage.cycleCapacity.status.belowPace", "低于匀速基准"),
        comparison: t("usage.cycleCapacity.comparison.belowPace", "低于基准"),
        className:
          "border-blue-500/25 bg-blue-500/10 text-blue-600 dark:text-blue-400",
      };
    case "on_pace":
      return {
        label: t("usage.cycleCapacity.status.onPace", "接近匀速基准"),
        comparison: t("usage.cycleCapacity.comparison.onPace", "基本一致"),
        className:
          "border-emerald-500/25 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
      };
    case "above_pace":
      return {
        label: t("usage.cycleCapacity.status.abovePace", "高于匀速基准"),
        comparison: t("usage.cycleCapacity.comparison.abovePace", "用量偏高"),
        className:
          "border-amber-500/25 bg-amber-500/10 text-amber-600 dark:text-amber-400",
      };
    case "far_above_pace":
      return {
        label: t("usage.cycleCapacity.status.farAbovePace", "明显高于匀速基准"),
        comparison: t(
          "usage.cycleCapacity.comparison.farAbovePace",
          "明显偏高",
        ),
        className:
          "border-red-500/25 bg-red-500/10 text-red-600 dark:text-red-400",
      };
    case "exhausted":
      return {
        label: t("usage.cycleCapacity.status.exhausted", "额度已耗尽"),
        comparison: t("usage.cycleCapacity.comparison.exhausted", "已耗尽"),
        className:
          "border-red-500/25 bg-red-500/10 text-red-600 dark:text-red-400",
      };
  }
}

function formatExhaustion(
  exhaustion: CodexForecastExhaustion,
  locale: string,
  t: TFunction,
): string {
  if (exhaustion.kind === "unavailable") {
    return t(
      "usage.cycleCapacity.waitingForSample",
      "采样中（至少需要 1 小时）",
    );
  }
  if (exhaustion.kind === "never") {
    return t(
      "usage.cycleCapacity.noExhaustionAtCurrentRate",
      "按当前速率不会耗尽",
    );
  }
  if (!exhaustion.withinCycle) {
    return t(
      "usage.cycleCapacity.noExhaustionBeforeReset",
      "本周期重置前不会耗尽",
    );
  }
  return formatDateTime(exhaustion.atMs, locale);
}

export function CodexCycleForecastPanel({
  forecast,
  resetAtMs,
  locale,
  lang,
  t,
}: {
  forecast: CodexCycleForecast;
  resetAtMs: number;
  locale: string;
  lang: string;
  t: TFunction;
}) {
  const status = statusPresentation(forecast.statusLevel, t);
  const perDay = t("usage.cycleCapacity.perDay", "%/天");
  const durationUnits = {
    day: t("usage.cycleCapacity.duration.day", "天"),
    hour: t("usage.cycleCapacity.duration.hour", "小时"),
    minute: t("usage.cycleCapacity.duration.minute", "分钟"),
  };
  const elapsed = formatDuration(forecast.elapsedMs, lang, durationUnits);
  const remaining = formatDuration(forecast.remainingMs, lang, durationUnits);
  const recentSpan =
    forecast.recentWindowSpanMs == null
      ? null
      : formatDuration(forecast.recentWindowSpanMs, lang, durationUnits);
  const waitingForSample = t(
    "usage.cycleCapacity.waitingForSample",
    "采样中（至少需要 1 小时）",
  );

  const timeProgressValue = t("usage.cycleCapacity.timeProgressValue", {
    defaultValue: "{{progress}} · 已过 {{elapsed}}",
    progress: formatPercent(forecast.cycleTimeProgressPercent),
    elapsed,
  });
  const actualVsBaselineValue = t("usage.cycleCapacity.actualVsBaselineValue", {
    defaultValue: "{{actual}} / {{baseline}}（{{comparison}}）",
    actual: formatPercent(forecast.currentUtilizationPercent),
    baseline: formatPercent(forecast.baselineUtilizationPercent),
    comparison: status.comparison,
  });
  const cumulativeVsSustainableValue = `${formatRate(
    forecast.cumulativeRatePercentPerDay,
    perDay,
  )} / ${formatRate(forecast.sustainableRatePercentPerDay, perDay)}`;
  const recentRateValue =
    forecast.recentRatePercentPerDay == null || recentSpan == null
      ? waitingForSample
      : t("usage.cycleCapacity.recentRateValue", {
          defaultValue: "{{rate}} · 采用最近 {{duration}} 的采样",
          rate: formatRate(forecast.recentRatePercentPerDay, perDay),
          duration: recentSpan,
        });
  const recentExhaustsEarly =
    forecast.recentExhaustion.kind === "at" &&
    forecast.recentExhaustion.atMs < resetAtMs - EXHAUSTION_TIME_EPSILON_MS;
  const projectedFinalValue =
    forecast.recentProjectedUtilizationAtReset == null
      ? waitingForSample
      : t("usage.cycleCapacity.projectedFinalValue", {
          defaultValue: "{{value}}{{suffix}}",
          value: formatPercent(forecast.recentProjectedUtilizationAtReset),
          suffix: recentExhaustsEarly
            ? t("usage.cycleCapacity.projectedExhaustEarly", "（预计提前耗尽）")
            : "",
        });

  return (
    <section
      className="mt-4 rounded-xl border border-border/50 bg-background/30 p-3.5 sm:p-4"
      data-testid="codex-cycle-forecast"
    >
      <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2 text-sm font-semibold">
          <Activity className="h-4 w-4 text-muted-foreground" />
          <h4>{t("usage.cycleCapacity.currentStatus", "当前状态")}</h4>
        </div>
        <span
          className={cn(
            "inline-flex items-center rounded-full border px-2.5 py-1 text-[11px] font-semibold",
            status.className,
          )}
          data-status={forecast.statusLevel}
          role="status"
          aria-live="polite"
        >
          {status.label}
        </span>
      </div>

      <dl className="grid grid-cols-1 gap-x-7 lg:grid-cols-2">
        <ForecastRow
          label={t("usage.cycleCapacity.timeProgress", "周期时间进度")}
          value={timeProgressValue}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.actualVsBaseline",
            "实际用量 / 基准用量（同期）",
          )}
          value={actualVsBaselineValue}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.cumulativeVsSustainableRate",
            "累计平均速率 / 可持续速率",
          )}
          value={cumulativeVsSustainableValue}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.recentRate",
            "近期消耗速率（采样区间）",
          )}
          value={recentRateValue}
          valueClassName={cn(
            forecast.recentRatePercentPerDay == null &&
              "font-medium text-muted-foreground",
          )}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.cumulativeExhaustionAt",
            "累计平均模型预计耗尽时间",
          )}
          value={formatExhaustion(forecast.cumulativeExhaustion, locale, t)}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.recentExhaustionAt",
            "近期速率模型预计耗尽时间",
          )}
          value={formatExhaustion(forecast.recentExhaustion, locale, t)}
          valueClassName={cn(
            forecast.recentExhaustion.kind === "unavailable" &&
              "font-medium text-muted-foreground",
          )}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.projectedFinalUtilization",
            "近期速率模型期末预计用量",
          )}
          value={projectedFinalValue}
          valueClassName={cn(
            forecast.recentProjectedUtilizationAtReset == null &&
              "font-medium text-muted-foreground",
            recentExhaustsEarly && "text-red-600 dark:text-red-400",
          )}
        />
        <ForecastRow
          label={t("usage.cycleCapacity.scheduledResetAt", "周期计划重置时间")}
          value={formatDateTime(resetAtMs, locale)}
        />
        <ForecastRow
          label={t(
            "usage.cycleCapacity.remainingToReset",
            "预测时点至重置的剩余时间",
          )}
          value={remaining}
        />
      </dl>
    </section>
  );
}
