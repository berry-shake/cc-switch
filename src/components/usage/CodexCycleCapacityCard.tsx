import { useId } from "react";
import { useTranslation } from "react-i18next";
import { CalendarClock, CircleDollarSign, Gauge, Info } from "lucide-react";

import { Card, CardContent } from "@/components/ui/card";
import {
  deriveCodexCycleCapacity,
  type CodexCycleCapacityEstimate,
} from "@/lib/codexCycleCapacity";
import { forecastCodexCycle } from "@/lib/codexCycleForecast";
import {
  getCodexQuotaSamplesForCycle,
  type CodexQuotaSample,
} from "@/lib/codexQuotaSamples";
import { cn } from "@/lib/utils";
import type { SubscriptionQuota } from "@/types/subscription";
import type { UsageSummary } from "@/types/usage";

import {
  fmtUsd,
  formatTokensShort,
  getLocaleFromLanguage,
  getResolvedLang,
} from "./format";
import { CodexCycleForecastPanel } from "./CodexCycleForecastPanel";

export interface CodexCycleCapacityCardProps {
  quota: SubscriptionQuota | null | undefined;
  /** 必须是 `quota` 当前长周期精确起止范围内的本地 Codex 汇总。 */
  usage: UsageSummary | null | undefined;
  /** 当前本机保存的官方额度采样；仅同一周期的数据会参与近期预测。 */
  quotaSamples?: readonly CodexQuotaSample[];
  className?: string;
  /** 仅用于可重复测试；生产环境使用当前时间。 */
  nowMs?: number;
}

function formatPercent(value: number): string {
  const rounded = Math.round(value);
  return Math.abs(value - rounded) < 0.05
    ? `${rounded}%`
    : `${value.toFixed(1)}%`;
}

function formatCycleDateTime(timestamp: number, locale: string): string {
  return new Intl.DateTimeFormat(locale, {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(timestamp));
}

function formatEstimatedTokens(value: number, lang: string): string {
  // 消除 1.785 这类二进制浮点表示略小于十进制半值导致的视觉向下舍入。
  const stabilized = value + Math.max(1, Math.abs(value)) * Number.EPSILON * 4;
  return formatTokensShort(stabilized, lang, 2);
}

interface CapacityMetricProps {
  label: string;
  value: string;
  title?: string;
  emphasized?: boolean;
}

function CapacityMetric({
  label,
  value,
  title,
  emphasized = false,
}: CapacityMetricProps) {
  return (
    <div
      className={cn(
        "min-w-0 rounded-xl border border-border/50 bg-background/45 p-3.5 shadow-sm",
        emphasized &&
          "border-emerald-500/20 bg-emerald-500/[0.045] dark:bg-emerald-500/[0.07]",
      )}
    >
      <div className="mb-1.5 text-[11px] font-medium leading-snug text-muted-foreground">
        {label}
      </div>
      <div
        className={cn(
          "truncate text-lg font-bold tabular-nums tracking-tight",
          emphasized && "text-emerald-600 dark:text-emerald-400",
        )}
        title={title ?? value}
      >
        {value}
      </div>
    </div>
  );
}

function CapacityRing({
  estimate,
  gradientId,
  usedLabel,
  estimateLabel,
}: {
  estimate: CodexCycleCapacityEstimate;
  gradientId: string;
  usedLabel: string;
  estimateLabel: string;
}) {
  const radius = 72;
  const circumference = 2 * Math.PI * radius;
  const progress = Math.max(0, Math.min(1, estimate.usedRatio));
  const dashOffset = circumference * (1 - progress);

  return (
    <div className="relative mx-auto h-44 w-44 shrink-0">
      <svg
        className="h-full w-full -rotate-90"
        viewBox="0 0 176 176"
        role="progressbar"
        aria-label={usedLabel}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={estimate.utilizationPercent}
      >
        <defs>
          <linearGradient id={gradientId} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0%" stopColor="#10b981" />
            <stop offset="100%" stopColor="#3b82f6" />
          </linearGradient>
        </defs>
        <circle
          cx="88"
          cy="88"
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeWidth="14"
          className="text-muted/70"
        />
        <circle
          cx="88"
          cy="88"
          r={radius}
          fill="none"
          stroke={`url(#${gradientId})`}
          strokeWidth="14"
          strokeLinecap="round"
          strokeDasharray={circumference}
          strokeDashoffset={dashOffset}
        />
      </svg>
      <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center">
        <span className="text-4xl font-black tabular-nums tracking-tight">
          {formatPercent(estimate.utilizationPercent)}
        </span>
        <span className="mt-1 text-[11px] font-medium text-muted-foreground">
          {usedLabel} · {estimateLabel}
        </span>
      </div>
    </div>
  );
}

/**
 * 纯展示组件。额度请求失败、长周期无效、已用比例为 0，或同期本地汇总为空时
 * 会完全隐藏，不保留空卡片或错误入口。
 */
export function CodexCycleCapacityCard({
  quota,
  usage,
  quotaSamples = [],
  className,
  nowMs,
}: CodexCycleCapacityCardProps) {
  const { t, i18n } = useTranslation();
  const lang = getResolvedLang(i18n);
  const locale = getLocaleFromLanguage(lang);
  const estimate = deriveCodexCycleCapacity(quota, usage, nowMs);
  const rawId = useId();
  const gradientId = `codex-capacity-${rawId.replace(/[^a-zA-Z0-9_-]/g, "")}`;

  if (!estimate) return null;

  const forecastSamples = getCodexQuotaSamplesForCycle(
    quotaSamples,
    estimate,
  ).map((sample) => ({
    timestampMs: sample.capturedAtMs,
    utilizationPercent: sample.utilizationPercent,
  }));
  const forecast = forecastCodexCycle({
    cycleStartMs: estimate.startMs,
    resetAtMs: estimate.resetAtMs,
    queriedAtMs: estimate.endMs,
    utilizationPercent: estimate.utilizationPercent,
    samples: forecastSamples,
  });

  const usedLabel = t("usage.cycleCapacity.used", "已使用");
  const estimateLabel = t("usage.cycleCapacity.estimate", "等效估算");
  const cycleRange = `${formatCycleDateTime(
    estimate.startMs,
    locale,
  )} → ${formatCycleDateTime(estimate.resetAtMs, locale)}`;

  return (
    <Card
      className={cn(
        "overflow-hidden border border-border/50 bg-card/60 shadow-sm backdrop-blur-xl",
        className,
      )}
      data-testid="codex-cycle-capacity-card"
    >
      <CardContent className="p-4 md:p-5">
        <div className="mb-4 flex flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
          <div className="flex min-w-0 items-start gap-3">
            <div className="rounded-xl bg-emerald-500/10 p-2.5 text-emerald-600 dark:text-emerald-400">
              <Gauge className="h-5 w-5" />
            </div>
            <div className="min-w-0">
              <h3 className="text-base font-semibold tracking-tight">
                {t("usage.cycleCapacity.title", "Codex 周期等效容量（估算）")}
              </h3>
              <div className="mt-1 flex items-center gap-1.5 text-[11px] text-muted-foreground">
                <CalendarClock className="h-3.5 w-3.5 shrink-0" />
                <span className="truncate" title={cycleRange}>
                  {cycleRange}
                </span>
              </div>
            </div>
          </div>
          <div className="flex items-center gap-1.5 rounded-lg border border-border/40 bg-background/40 px-2.5 py-1.5 text-[10px] leading-tight text-muted-foreground">
            <Info className="h-3.5 w-3.5 shrink-0" />
            {t(
              "usage.cycleCapacity.basis",
              "按当前模型、速度及 Token 结构折算",
            )}
          </div>
        </div>

        <div className="grid gap-5 lg:grid-cols-[190px_minmax(0,1fr)] lg:items-center">
          <CapacityRing
            estimate={estimate}
            gradientId={gradientId}
            usedLabel={usedLabel}
            estimateLabel={estimateLabel}
          />

          <div className="min-w-0">
            <div className="mb-4 h-2 overflow-hidden rounded-full bg-muted/70">
              <div
                className="h-full rounded-full bg-gradient-to-r from-emerald-500 to-blue-500"
                style={{
                  width: `${Math.min(100, estimate.utilizationPercent)}%`,
                }}
              />
            </div>

            <div className="grid grid-cols-1 gap-2.5 sm:grid-cols-2 xl:grid-cols-3">
              <CapacityMetric
                label={t(
                  "usage.cycleCapacity.remainingQuota",
                  "剩余额度（估算）",
                )}
                value={formatPercent(estimate.remainingPercent)}
              />
              <CapacityMetric
                label={t(
                  "usage.cycleCapacity.totalTokens",
                  "完整周期 Token 等效容量（估算）",
                )}
                value={formatEstimatedTokens(estimate.totalTokens, lang)}
                title={Math.round(estimate.totalTokens).toLocaleString(locale)}
                emphasized
              />
              <CapacityMetric
                label={t(
                  "usage.cycleCapacity.usedUsd",
                  "当前累计估算费用（USD）",
                )}
                value={fmtUsd(estimate.usedUsd, 2)}
              />
              <CapacityMetric
                label={t(
                  "usage.cycleCapacity.totalUsd",
                  "完整周期美元等效容量（估算）",
                )}
                value={fmtUsd(estimate.totalUsd, 2)}
                emphasized
              />
              <CapacityMetric
                label={t(
                  "usage.cycleCapacity.remainingTokens",
                  "剩余额度 Token 等效容量（估算）",
                )}
                value={formatEstimatedTokens(estimate.remainingTokens, lang)}
                title={Math.round(estimate.remainingTokens).toLocaleString(
                  locale,
                )}
              />
              <CapacityMetric
                label={t(
                  "usage.cycleCapacity.remainingUsd",
                  "剩余额度美元等效容量（估算）",
                )}
                value={fmtUsd(estimate.remainingUsd, 2)}
              />
            </div>
          </div>
        </div>

        {forecast ? (
          <CodexCycleForecastPanel
            forecast={forecast}
            resetAtMs={estimate.resetAtMs}
            locale={locale}
            lang={lang}
            t={t}
          />
        ) : null}

        <div className="mt-4 flex items-start gap-2 rounded-lg bg-muted/30 px-3 py-2 text-[10px] leading-relaxed text-muted-foreground">
          <CircleDollarSign className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <span>
            {t(
              "usage.cycleCapacity.disclaimer",
              "这是本机单账号用量按官方已用比例反推的 USD/Token 等效值，受本地日志完整性与同步延迟影响，不代表官方账单或固定 Token 上限。",
            )}
          </span>
        </div>
      </CardContent>
    </Card>
  );
}
