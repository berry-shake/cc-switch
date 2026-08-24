import type { TFunction } from "i18next";
import { Activity } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import type {
  CodexCycleForecast,
  CodexForecastExhaustion,
  CodexForecastStatusLevel,
} from "@/lib/codexCycleForecast";
import { cn } from "@/lib/utils";

const EXHAUSTION_TIME_EPSILON_MS = 1_000;
const DAY_MS = 24 * 60 * 60 * 1000;

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

type ForecastMetricTone = "neutral" | "info" | "success" | "warning" | "danger";

const FORECAST_METRIC_TONE_CLASS: Record<ForecastMetricTone, string> = {
  neutral: "border-border/50 bg-background/45",
  info: "border-blue-500/20 bg-blue-500/[0.045] dark:bg-blue-500/[0.07]",
  success:
    "border-emerald-500/20 bg-emerald-500/[0.045] dark:bg-emerald-500/[0.07]",
  warning: "border-amber-500/25 bg-amber-500/[0.055] dark:bg-amber-500/[0.08]",
  danger: "border-red-500/25 bg-red-500/[0.055] dark:bg-red-500/[0.08]",
};

const FORECAST_METRIC_VALUE_CLASS: Record<ForecastMetricTone, string> = {
  neutral: "text-foreground",
  info: "text-blue-600 dark:text-blue-400",
  success: "text-emerald-600 dark:text-emerald-400",
  warning: "text-amber-600 dark:text-amber-400",
  danger: "text-red-600 dark:text-red-400",
};

const FORECAST_METRIC_PROGRESS_CLASS: Record<ForecastMetricTone, string> = {
  neutral: "bg-gradient-to-r from-emerald-500 via-cyan-500 to-blue-500",
  info: "bg-blue-500",
  success: "bg-emerald-500",
  warning: "bg-amber-500",
  danger: "bg-red-500",
};

function ForecastMetric({
  metricId,
  label,
  value,
  tone = "neutral",
  primary = false,
  className,
  valueClassName,
  progressPercent,
  markerPercent,
}: {
  metricId: string;
  label: string;
  value: string;
  tone?: ForecastMetricTone;
  primary?: boolean;
  className?: string;
  valueClassName?: string;
  progressPercent?: number;
  markerPercent?: number;
}) {
  const safeProgress =
    progressPercent == null
      ? null
      : Math.max(0, Math.min(100, progressPercent));
  const safeMarker =
    markerPercent == null ? null : Math.max(0, Math.min(100, markerPercent));

  return (
    <div
      className={cn(
        "flex min-w-0 flex-col rounded-xl border shadow-sm",
        primary ? "min-h-[112px] p-4" : "min-h-[92px] p-3.5",
        FORECAST_METRIC_TONE_CLASS[tone],
        className,
      )}
      data-forecast-metric={metricId}
      data-prominence={primary ? "primary" : "secondary"}
      data-tone={tone}
    >
      <dt className="text-[11px] font-medium leading-snug text-muted-foreground">
        {label}
      </dt>
      <dd
        className={cn(
          "mt-2 flex flex-1 flex-col break-words font-bold leading-snug tabular-nums tracking-tight",
          primary ? "text-xl md:text-2xl" : "text-base",
          FORECAST_METRIC_VALUE_CLASS[tone],
          valueClassName,
        )}
      >
        <span className="block">{value}</span>
        {safeProgress != null ? (
          <div
            className="mt-auto pt-3"
            role="progressbar"
            aria-label={label}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={safeProgress}
            aria-valuetext={value}
          >
            <div className="relative h-1.5 rounded-full bg-muted/80">
              <div
                className={cn(
                  "h-full rounded-full",
                  FORECAST_METRIC_PROGRESS_CLASS[tone],
                )}
                style={{ width: `${safeProgress}%` }}
              />
              {safeMarker != null ? (
                <span
                  className="absolute top-1/2 h-2.5 w-0.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-foreground/75 ring-2 ring-background"
                  style={{ left: `${safeMarker}%` }}
                  data-baseline-marker
                  aria-hidden="true"
                />
              ) : null}
            </div>
          </div>
        ) : null}
      </dd>
    </div>
  );
}

function ForecastDetail({
  metricId,
  label,
  value,
  tone = "neutral",
  valueClassName,
}: {
  metricId: string;
  label: string;
  value: string;
  tone?: ForecastMetricTone;
  valueClassName?: string;
}) {
  return (
    <div
      className="min-w-0"
      data-forecast-metric={metricId}
      data-prominence="secondary"
      data-tone={tone}
    >
      <dt className="text-[11px] font-medium leading-snug text-muted-foreground">
        {label}
      </dt>
      <dd
        className={cn(
          "mt-1.5 break-words text-sm font-semibold leading-snug tabular-nums",
          FORECAST_METRIC_VALUE_CLASS[tone],
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
): {
  label: string;
  comparison: string;
  className: string;
  iconClassName: string;
  metricTone: ForecastMetricTone;
} {
  switch (status) {
    case "below_pace":
      return {
        label: t("usage.cycleCapacity.status.belowPace", "低于匀速基准"),
        comparison: t("usage.cycleCapacity.comparison.belowPace", "低于基准"),
        className:
          "border-blue-500/25 bg-blue-500/10 text-blue-600 dark:text-blue-400",
        iconClassName: "bg-blue-500/10 text-blue-600 dark:text-blue-400",
        metricTone: "info",
      };
    case "on_pace":
      return {
        label: t("usage.cycleCapacity.status.onPace", "接近匀速基准"),
        comparison: t("usage.cycleCapacity.comparison.onPace", "基本一致"),
        className:
          "border-emerald-500/25 bg-emerald-500/10 text-emerald-700 dark:text-emerald-400",
        iconClassName:
          "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
        metricTone: "success",
      };
    case "above_pace":
      return {
        label: t("usage.cycleCapacity.status.abovePace", "高于匀速基准"),
        comparison: t("usage.cycleCapacity.comparison.abovePace", "用量偏高"),
        className:
          "border-amber-500/25 bg-amber-500/10 text-amber-700 dark:text-amber-400",
        iconClassName: "bg-amber-500/10 text-amber-600 dark:text-amber-400",
        metricTone: "warning",
      };
    case "far_above_pace":
      return {
        label: t("usage.cycleCapacity.status.farAbovePace", "明显高于匀速基准"),
        comparison: t(
          "usage.cycleCapacity.comparison.farAbovePace",
          "明显偏高",
        ),
        className:
          "border-red-500/25 bg-red-500/10 text-red-700 dark:text-red-400",
        iconClassName: "bg-red-500/10 text-red-600 dark:text-red-400",
        metricTone: "danger",
      };
    case "exhausted":
      return {
        label: t("usage.cycleCapacity.status.exhausted", "额度已耗尽"),
        comparison: t("usage.cycleCapacity.comparison.exhausted", "已耗尽"),
        className:
          "border-red-500/25 bg-red-500/10 text-red-700 dark:text-red-400",
        iconClassName: "bg-red-500/10 text-red-600 dark:text-red-400",
        metricTone: "danger",
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

function CodexCycleForecastWaitingPanel({
  cycleStartMs,
  queriedAtMs,
  resetAtMs,
  waitingTitle,
  waitingDescription,
  locale,
  lang,
  t,
}: {
  cycleStartMs: number;
  queriedAtMs: number;
  resetAtMs: number;
  waitingTitle: string;
  waitingDescription: string;
  locale: string;
  lang: string;
  t: TFunction;
}) {
  const cycleDurationMs = Math.max(1, resetAtMs - cycleStartMs);
  const elapsedMs = Math.min(
    cycleDurationMs,
    Math.max(0, queriedAtMs - cycleStartMs),
  );
  const remainingMs = Math.max(0, resetAtMs - queriedAtMs);
  const cycleTimeProgressPercent = (elapsedMs / cycleDurationMs) * 100;
  const sustainableRatePercentPerDay = 100 / (cycleDurationMs / DAY_MS);
  const perDay = t("usage.cycleCapacity.perDay", "%/天");
  const waitingMetricValue = t(
    "usage.cycleCapacity.empty.metricValue",
    "待估算",
  );
  const durationUnits = {
    day: t("usage.cycleCapacity.duration.day", "天"),
    hour: t("usage.cycleCapacity.duration.hour", "小时"),
    minute: t("usage.cycleCapacity.duration.minute", "分钟"),
  };
  const elapsed = formatDuration(elapsedMs, lang, durationUnits);
  const remaining = formatDuration(remainingMs, lang, durationUnits);
  const timeProgressValue = t("usage.cycleCapacity.timeProgressValue", {
    defaultValue: "{{progress}} · 已过 {{elapsed}}",
    progress: formatPercent(cycleTimeProgressPercent),
    elapsed,
  });

  return (
    <section
      className="mt-5 border-t border-border/50 pt-5"
      data-testid="codex-cycle-forecast"
      data-state="waiting-utilization"
    >
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-3">
          <div className="rounded-xl bg-muted/70 p-2.5 text-muted-foreground">
            <Activity className="h-5 w-5" />
          </div>
          <h4 className="text-base font-semibold tracking-tight">
            {t("usage.cycleCapacity.currentStatus", "当前状态")}
          </h4>
        </div>
        <Badge
          variant="outline"
          className="inline-flex items-center rounded-full border border-border/60 bg-muted/50 px-3 py-1.5 text-xs font-semibold text-muted-foreground shadow-sm"
          data-status="waiting_utilization"
          role="status"
          aria-live="polite"
        >
          {waitingTitle}
        </Badge>
      </div>

      <dl
        className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3"
        data-testid="codex-cycle-forecast-overview"
      >
        <ForecastMetric
          metricId="actual-vs-baseline"
          label={t(
            "usage.cycleCapacity.actualVsBaseline",
            "实际用量 / 基准用量（同期）",
          )}
          value={waitingMetricValue}
          primary
        />
        <ForecastMetric
          metricId="time-progress"
          label={t("usage.cycleCapacity.timeProgress", "周期时间进度")}
          value={timeProgressValue}
          primary
          progressPercent={cycleTimeProgressPercent}
        />
        <ForecastMetric
          metricId="cumulative-vs-sustainable-rate"
          label={t(
            "usage.cycleCapacity.cumulativeVsSustainableRate",
            "累计平均速率 / 可持续速率",
          )}
          value={`${waitingMetricValue} / ${formatRate(
            sustainableRatePercentPerDay,
            perDay,
          )}`}
          primary
          className="sm:col-span-2 xl:col-span-1"
        />
      </dl>

      <div className="mt-3 grid grid-cols-1 gap-3 lg:grid-cols-[minmax(0,2fr)_minmax(260px,1fr)]">
        <div
          className="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2"
          data-testid="codex-cycle-forecast-models"
        >
          <div
            className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-4 shadow-sm"
            data-testid="codex-cycle-forecast-recent-model"
          >
            <div
              className="mb-4 rounded-lg border border-border/40 bg-muted/30 px-3 py-2 text-xs font-medium text-muted-foreground"
              data-testid="codex-cycle-forecast-waiting-utilization"
              role="status"
              aria-live="polite"
            >
              {waitingDescription}
            </div>
            <dl className="grid grid-cols-1 gap-4">
              <ForecastDetail
                metricId="recent-rate"
                label={t(
                  "usage.cycleCapacity.recentRate",
                  "近期消耗速率（采样区间）",
                )}
                value={waitingMetricValue}
                valueClassName="font-medium text-muted-foreground"
              />
              <ForecastDetail
                metricId="projected-final-utilization"
                label={t(
                  "usage.cycleCapacity.projectedFinalUtilization",
                  "近期速率模型期末预计用量",
                )}
                value={waitingMetricValue}
                valueClassName="font-medium text-muted-foreground"
              />
            </dl>
          </div>

          <div
            className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-4 shadow-sm"
            data-testid="codex-cycle-forecast-exhaustion-models"
          >
            <dl className="grid grid-cols-1 gap-4">
              <ForecastDetail
                metricId="cumulative-exhaustion-at"
                label={t(
                  "usage.cycleCapacity.cumulativeExhaustionAt",
                  "累计平均模型预计耗尽时间",
                )}
                value={waitingMetricValue}
                valueClassName="font-medium text-muted-foreground"
              />
              <ForecastDetail
                metricId="recent-exhaustion-at"
                label={t(
                  "usage.cycleCapacity.recentExhaustionAt",
                  "近期速率模型预计耗尽时间",
                )}
                value={waitingMetricValue}
                valueClassName="font-medium text-muted-foreground"
              />
            </dl>
          </div>
        </div>

        <div
          className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-4 shadow-sm"
          data-testid="codex-cycle-forecast-reset"
        >
          <dl className="grid h-full grid-cols-1 gap-4">
            <ForecastDetail
              metricId="scheduled-reset-at"
              label={t(
                "usage.cycleCapacity.scheduledResetAt",
                "周期计划重置时间",
              )}
              value={formatDateTime(resetAtMs, locale)}
              valueClassName="text-base"
            />
            <ForecastDetail
              metricId="remaining-to-reset"
              label={t(
                "usage.cycleCapacity.remainingToReset",
                "预测时点至重置的剩余时间",
              )}
              value={remaining}
              valueClassName="text-base"
            />
          </dl>
        </div>
      </div>
    </section>
  );
}

export function CodexCycleForecastPanel({
  forecast,
  cycleStartMs,
  queriedAtMs,
  resetAtMs,
  waitingTitle,
  waitingDescription,
  locale,
  lang,
  t,
}: {
  forecast: CodexCycleForecast | null;
  cycleStartMs: number;
  queriedAtMs: number;
  resetAtMs: number;
  waitingTitle: string;
  waitingDescription: string;
  locale: string;
  lang: string;
  t: TFunction;
}) {
  if (!forecast) {
    return (
      <CodexCycleForecastWaitingPanel
        cycleStartMs={cycleStartMs}
        queriedAtMs={queriedAtMs}
        resetAtMs={resetAtMs}
        waitingTitle={waitingTitle}
        waitingDescription={waitingDescription}
        locale={locale}
        lang={lang}
        t={t}
      />
    );
  }

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
  const recentPredictionUnavailable =
    forecast.recentRatePercentPerDay == null || recentSpan == null;
  const recentRateValue =
    forecast.recentRatePercentPerDay == null || recentSpan == null
      ? "—"
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
      ? "—"
      : t("usage.cycleCapacity.projectedFinalValue", {
          defaultValue: "{{value}}{{suffix}}",
          value: formatPercent(forecast.recentProjectedUtilizationAtReset),
          suffix: recentExhaustsEarly
            ? t("usage.cycleCapacity.projectedExhaustEarly", "（预计提前耗尽）")
            : "",
        });
  const recentExhaustionValue = recentPredictionUnavailable
    ? "—"
    : formatExhaustion(forecast.recentExhaustion, locale, t);

  return (
    <section
      className="mt-5 border-t border-border/50 pt-5"
      data-testid="codex-cycle-forecast"
    >
      <div className="mb-4 flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-3">
          <div className={cn("rounded-xl p-2.5", status.iconClassName)}>
            <Activity className="h-5 w-5" />
          </div>
          <h4 className="text-base font-semibold tracking-tight">
            {t("usage.cycleCapacity.currentStatus", "当前状态")}
          </h4>
        </div>
        <Badge
          variant="outline"
          className={cn(
            "inline-flex items-center rounded-full border px-3 py-1.5 text-xs font-semibold shadow-sm",
            status.className,
          )}
          data-status={forecast.statusLevel}
          role="status"
          aria-live="polite"
        >
          {status.label}
        </Badge>
      </div>

      <dl
        className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3"
        data-testid="codex-cycle-forecast-overview"
      >
        <ForecastMetric
          metricId="actual-vs-baseline"
          label={t(
            "usage.cycleCapacity.actualVsBaseline",
            "实际用量 / 基准用量（同期）",
          )}
          value={actualVsBaselineValue}
          tone={status.metricTone}
          primary
          progressPercent={forecast.currentUtilizationPercent}
          markerPercent={forecast.baselineUtilizationPercent}
        />
        <ForecastMetric
          metricId="time-progress"
          label={t("usage.cycleCapacity.timeProgress", "周期时间进度")}
          value={timeProgressValue}
          primary
          progressPercent={forecast.cycleTimeProgressPercent}
        />
        <ForecastMetric
          metricId="cumulative-vs-sustainable-rate"
          label={t(
            "usage.cycleCapacity.cumulativeVsSustainableRate",
            "累计平均速率 / 可持续速率",
          )}
          value={cumulativeVsSustainableValue}
          primary
          className="sm:col-span-2 xl:col-span-1"
        />
      </dl>

      <div className="mt-3 grid grid-cols-1 gap-3 lg:grid-cols-[minmax(0,2fr)_minmax(260px,1fr)]">
        <div
          className="grid min-w-0 grid-cols-1 gap-3 sm:grid-cols-2"
          data-testid="codex-cycle-forecast-models"
        >
          <div
            className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-4 shadow-sm"
            data-testid="codex-cycle-forecast-recent-model"
          >
            {recentPredictionUnavailable ? (
              <div
                className="mb-4 rounded-lg border border-border/40 bg-muted/30 px-3 py-2 text-xs font-medium text-muted-foreground"
                data-testid="codex-cycle-recent-sampling"
                role="status"
                aria-live="polite"
              >
                {waitingForSample}
              </div>
            ) : null}

            <dl className="grid grid-cols-1 gap-4">
              <ForecastDetail
                metricId="recent-rate"
                label={t(
                  "usage.cycleCapacity.recentRate",
                  "近期消耗速率（采样区间）",
                )}
                value={recentRateValue}
                valueClassName={cn(
                  recentPredictionUnavailable &&
                    "font-medium text-muted-foreground",
                )}
              />
              <ForecastDetail
                metricId="projected-final-utilization"
                label={t(
                  "usage.cycleCapacity.projectedFinalUtilization",
                  "近期速率模型期末预计用量",
                )}
                value={projectedFinalValue}
                tone={recentExhaustsEarly ? "danger" : "neutral"}
                valueClassName={cn(
                  recentPredictionUnavailable &&
                    "font-medium text-muted-foreground",
                )}
              />
            </dl>
          </div>

          <div
            className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-4 shadow-sm"
            data-testid="codex-cycle-forecast-exhaustion-models"
          >
            <dl className="grid grid-cols-1 gap-4">
              <ForecastDetail
                metricId="cumulative-exhaustion-at"
                label={t(
                  "usage.cycleCapacity.cumulativeExhaustionAt",
                  "累计平均模型预计耗尽时间",
                )}
                value={formatExhaustion(
                  forecast.cumulativeExhaustion,
                  locale,
                  t,
                )}
              />
              <ForecastDetail
                metricId="recent-exhaustion-at"
                label={t(
                  "usage.cycleCapacity.recentExhaustionAt",
                  "近期速率模型预计耗尽时间",
                )}
                value={recentExhaustionValue}
                valueClassName={cn(
                  recentPredictionUnavailable &&
                    "font-medium text-muted-foreground",
                )}
              />
            </dl>
          </div>
        </div>

        <div
          className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-4 shadow-sm"
          data-testid="codex-cycle-forecast-reset"
        >
          <dl className="grid h-full grid-cols-1 gap-4">
            <ForecastDetail
              metricId="scheduled-reset-at"
              label={t(
                "usage.cycleCapacity.scheduledResetAt",
                "周期计划重置时间",
              )}
              value={formatDateTime(resetAtMs, locale)}
              valueClassName="text-base"
            />
            <ForecastDetail
              metricId="remaining-to-reset"
              label={t(
                "usage.cycleCapacity.remainingToReset",
                "预测时点至重置的剩余时间",
              )}
              value={remaining}
              valueClassName="text-base"
            />
          </dl>
        </div>
      </div>
    </section>
  );
}
