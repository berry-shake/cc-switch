import { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  CalendarClock,
  ChevronDown,
  CircleDollarSign,
  Clock3,
  Gauge,
  Info,
  Mail,
  RefreshCw,
} from "lucide-react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  deriveCodexCycleCapacity,
  resolveCodexQuotaCycleWindow,
} from "@/lib/codexCycleCapacity";
import {
  deriveCodexAnalyticsCycleCapacity,
  type CodexAnalyticsCycleCapacityEstimate,
} from "@/lib/codexAnalyticsCapacity";
import { forecastCodexCycle } from "@/lib/codexCycleForecast";
import {
  CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY,
  CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
  persistCodexCycleCapacityMode,
  readCodexCycleCapacityMode,
  type CodexCycleCapacityCalculationMode,
} from "@/lib/codexCycleCapacityMode";
import {
  getCodexQuotaSamplesForCycle,
  type CodexQuotaSample,
} from "@/lib/codexQuotaSamples";
import { cn } from "@/lib/utils";
import type {
  CodexAnalyticsUsage,
  CodexQuotaWindow,
  SubscriptionQuota,
} from "@/types/subscription";
import type { ModelPricing, UsageSummary } from "@/types/usage";

import { fmtUsd, getLocaleFromLanguage, getResolvedLang } from "./format";
import { CodexCycleForecastPanel } from "./CodexCycleForecastPanel";

export interface CodexCycleCapacityCardProps {
  quota: SubscriptionQuota | null | undefined;
  /** Codex 专属原始窗口；可区分真实 0% 和尚未返回 used_percent。 */
  quotaWindows?: readonly CodexQuotaWindow[] | null;
  /** 与本次额度快照来自同一次官方响应的账号邮箱。 */
  accountEmail?: string | null;
  /** 本次额度快照完成查询的 Unix 毫秒时间戳。 */
  lastRefreshedAt?: number | null;
  /** 必须是 `quota` 当前长周期精确起止范围内的本地 Codex 汇总。 */
  usage: UsageSummary | null | undefined;
  /** 来自 Codex 官方用量接口的日级 Token 与模型/速度数据。 */
  analyticsUsage?: CodexAnalyticsUsage | null;
  /** 两种来源共用的本地模型定价；官方接口模式不会另起硬编码价格表。 */
  modelPricing?: readonly ModelPricing[] | null;
  /** 当前本机保存的官方额度采样；仅同一周期的数据会参与近期预测。 */
  quotaSamples?: readonly CodexQuotaSample[];
  /** 受控估算模式；未传入时卡片自行读取并保存本地偏好。 */
  calculationMode?: CodexCycleCapacityCalculationMode;
  onCalculationModeChange?: (mode: CodexCycleCapacityCalculationMode) => void;
  /** 仅在官方接口模式显示的手动刷新操作。 */
  onRefreshAnalytics?: () => void | Promise<void>;
  isRefreshingAnalytics?: boolean;
  className?: string;
  /** 仅用于可重复测试；生产环境使用当前时间。 */
  nowMs?: number;
}

export {
  CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY,
  CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
};
export type { CodexCycleCapacityCalculationMode };

function readInitialExpandedState(): boolean {
  if (typeof window === "undefined") return true;

  try {
    const stored = window.localStorage.getItem(
      CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY,
    );
    if (stored === "false") return false;
    if (stored === "true") return true;
  } catch {
    // localStorage may be unavailable in restricted webviews; keep the default.
  }

  return true;
}

function persistExpandedState(expanded: boolean): void {
  if (typeof window === "undefined") return;

  try {
    window.localStorage.setItem(
      CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY,
      String(expanded),
    );
  } catch {
    // The panel remains usable even when the preference cannot be persisted.
  }
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

function formatEstimatedTokens(value: number): string {
  // 消除 1.785 这类二进制浮点表示略小于十进制半值导致的视觉向下舍入。
  const stabilized = value + Math.max(1, Math.abs(value)) * Number.EPSILON * 4;
  if (!Number.isFinite(stabilized) || stabilized <= 0) return "0";
  if (stabilized >= 1e9) return `${(stabilized / 1e9).toFixed(2)}B`;
  if (stabilized >= 1e6) return `${(stabilized / 1e6).toFixed(2)}M`;
  if (stabilized >= 1e3) return `${(stabilized / 1e3).toFixed(2)}K`;
  return Math.round(stabilized).toLocaleString("en-US");
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
  utilizationPercent,
  gradientId,
  usedLabel,
  detailLabel,
}: {
  utilizationPercent: number | null;
  gradientId: string;
  usedLabel: string;
  detailLabel: string;
}) {
  const radius = 72;
  const circumference = 2 * Math.PI * radius;
  const progress =
    utilizationPercent == null
      ? 0
      : Math.max(0, Math.min(1, utilizationPercent / 100));
  const dashOffset = circumference * (1 - progress);
  const progressProps =
    utilizationPercent == null
      ? { role: "status" as const, "aria-label": detailLabel }
      : {
          role: "progressbar" as const,
          "aria-label": usedLabel,
          "aria-valuemin": 0,
          "aria-valuemax": 100,
          "aria-valuenow": utilizationPercent,
        };

  return (
    <div className="relative mx-auto h-44 w-44 shrink-0">
      <svg
        className="h-full w-full -rotate-90"
        viewBox="0 0 176 176"
        {...progressProps}
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
          {utilizationPercent == null ? "—" : formatPercent(utilizationPercent)}
        </span>
        <span className="mt-1 text-[11px] font-medium text-muted-foreground">
          {usedLabel} · {detailLabel}
        </span>
      </div>
    </div>
  );
}

/**
 * 纯展示组件。真实请求失败仍保持入口隐藏；接口成功后，即使周期刚重置、
 * used_percent 尚未同步或同期用量暂为空，也保留同一张卡片和刷新入口。
 */
export function CodexCycleCapacityCard({
  quota,
  quotaWindows,
  accountEmail,
  lastRefreshedAt,
  usage,
  analyticsUsage,
  modelPricing,
  quotaSamples = [],
  calculationMode,
  onCalculationModeChange,
  onRefreshAnalytics,
  isRefreshingAnalytics = false,
  className,
  nowMs,
}: CodexCycleCapacityCardProps) {
  const { t, i18n } = useTranslation();
  const lang = getResolvedLang(i18n);
  const locale = getLocaleFromLanguage(lang);
  const cycleWindow = resolveCodexQuotaCycleWindow(quota, quotaWindows, nowMs);
  const localEstimate = deriveCodexCycleCapacity(quota, usage, nowMs);
  const analyticsEstimate = deriveCodexAnalyticsCycleCapacity(
    quota,
    analyticsUsage,
    modelPricing,
    nowMs,
  );
  const [isExpanded, setIsExpanded] = useState(readInitialExpandedState);
  const [uncontrolledMode, setUncontrolledMode] = useState(
    readCodexCycleCapacityMode,
  );
  const selectedMode = calculationMode ?? uncontrolledMode;
  const rawId = useId();
  const gradientId = `codex-capacity-${rawId.replace(/[^a-zA-Z0-9_-]/g, "")}`;

  const effectiveMode: CodexCycleCapacityCalculationMode = selectedMode;
  const estimate =
    effectiveMode === "analytics" ? analyticsEstimate : localEstimate;

  if (!quota?.success) return null;

  const activeAnalyticsEstimate: CodexAnalyticsCycleCapacityEstimate | null =
    effectiveMode === "analytics" ? analyticsEstimate : null;
  const showModeSwitch = Boolean(
    onCalculationModeChange || (localEstimate && analyticsEstimate),
  );

  const forecast =
    cycleWindow?.utilizationPercent != null
      ? forecastCodexCycle({
          cycleStartMs: cycleWindow.startMs,
          resetAtMs: cycleWindow.resetAtMs,
          queriedAtMs: cycleWindow.endMs,
          utilizationPercent: cycleWindow.utilizationPercent,
          samples: getCodexQuotaSamplesForCycle(quotaSamples, cycleWindow).map(
            (sample) => ({
              timestampMs: sample.capturedAtMs,
              utilizationPercent: sample.utilizationPercent,
            }),
          ),
        })
      : null;

  const usedLabel = t("usage.cycleCapacity.used", "已用");
  const estimateLabel = t("usage.cycleCapacity.estimate", "等效估算");
  const titleLabel = t(
    "usage.cycleCapacity.title",
    "Codex 周期等效容量（估算）",
  );
  const toggleLabel = isExpanded
    ? t("usage.collapse", "收起")
    : t("usage.expand", "展开");
  const toggleAriaLabel = `${toggleLabel} ${titleLabel}`;
  const analyticsAccountMode =
    activeAnalyticsEstimate?.accountMode ?? analyticsUsage?.accountMode ?? null;
  const basisLabel =
    effectiveMode === "analytics"
      ? analyticsAccountMode === "workspace"
        ? t(
            "usage.cycleCapacity.analyticsWorkspaceBasis",
            "按官方用量接口的模型级 Token 与速度折算",
          )
        : analyticsAccountMode === "personal"
          ? t(
              "usage.cycleCapacity.analyticsPersonalBasis",
              "按官方用量接口的每日 Token 与模型/速度额度占比折算",
            )
          : t(
              "usage.cycleCapacity.analyticsWaitingBasis",
              "等待官方用量与模型/速度数据同步",
            )
      : t("usage.cycleCapacity.basis", "按当前模型、速度及 Token 结构折算");
  const compactBasisLabel =
    effectiveMode === "analytics"
      ? analyticsAccountMode === "workspace"
        ? t(
            "usage.cycleCapacity.analyticsWorkspaceCompactBasis",
            "官方模型 Token · 速度",
          )
        : analyticsAccountMode === "personal"
          ? t(
              "usage.cycleCapacity.analyticsPersonalCompactBasis",
              "官方每日 Token · 模型/速度",
            )
          : t(
              "usage.cycleCapacity.analyticsWaitingCompactBasis",
              "官方接口 · 等待同步",
            )
      : t("usage.cycleCapacity.compactBasis", "模型 · 速度 · Token 结构");
  const disclaimer =
    effectiveMode === "analytics"
      ? analyticsAccountMode === "workspace"
        ? t(
            "usage.cycleCapacity.analyticsWorkspaceDisclaimer",
            "这是官方用量接口返回的模型级日统计按已用比例反推的 USD/Token 等效值。日桶存在同步延迟，周期中途重置时起始日可能偏高；结果不代表官方账单或固定 Token 上限。",
          )
        : analyticsAccountMode === "personal"
          ? t(
              "usage.cycleCapacity.analyticsPersonalDisclaimer",
              "这是官方用量接口返回的每日总 Token 与模型/速度额度占比经费率校正后，按已用比例反推的等效值。多模型拆分、日桶同步及周期中途重置均会带来误差；结果不代表官方账单或固定 Token 上限。",
            )
          : t(
              "usage.cycleCapacity.analyticsWaitingDisclaimer",
              "官方周期或用量可能仍在同步；在已用比例和可定价用量同时有效前不会进行容量外推。",
            )
      : t(
          "usage.cycleCapacity.disclaimer",
          "这是本机单账号用量按官方已用比例反推的 USD/Token 等效值，受本地日志完整性与同步延迟影响，不代表官方账单或固定 Token 上限。",
        );
  const cycleRange = cycleWindow
    ? `${formatCycleDateTime(
        cycleWindow.startMs,
        locale,
      )} → ${formatCycleDateTime(cycleWindow.resetAtMs, locale)}`
    : t("usage.cycleCapacity.waitingCycleRange", "周期信息同步中");
  const utilizationPercent =
    estimate?.utilizationPercent ?? cycleWindow?.utilizationPercent ?? null;
  const waitingReason = !estimate
    ? !cycleWindow
      ? "cycle"
      : cycleWindow.utilizationPercent == null
        ? "utilization"
        : cycleWindow.utilizationPercent <= 0
          ? "firstUsage"
          : effectiveMode === "analytics"
            ? "analytics"
            : "local"
    : null;
  const waitingTitle =
    waitingReason === "cycle"
      ? t("usage.cycleCapacity.empty.cycleTitle", "等待周期数据")
      : waitingReason === "utilization"
        ? t("usage.cycleCapacity.empty.utilizationTitle", "等待额度比例")
        : waitingReason === "firstUsage"
          ? t("usage.cycleCapacity.empty.firstUsageTitle", "新周期已开始")
          : waitingReason === "analytics"
            ? t("usage.cycleCapacity.empty.analyticsTitle", "等待官方用量同步")
            : t("usage.cycleCapacity.empty.localTitle", "等待本地用量同步");
  const waitingDescription =
    waitingReason === "cycle"
      ? t(
          "usage.cycleCapacity.empty.cycleDescription",
          "官方接口暂未返回有效长周期，可能刚完成重置；稍后会自动重试，也可以手动刷新。",
        )
      : waitingReason === "utilization"
        ? t(
            "usage.cycleCapacity.empty.utilizationDescription",
            "已获取周期范围，但官方尚未同步已用比例；当前不会把缺失值当成 0%。",
          )
        : waitingReason === "firstUsage"
          ? t(
              "usage.cycleCapacity.empty.firstUsageDescription",
              "官方已用比例为 0%；首笔用量同步后，将自动计算 Token 和美元等效容量。",
            )
          : waitingReason === "analytics"
            ? t(
                "usage.cycleCapacity.empty.analyticsDescription",
                "额度比例已经可用，但官方 Token 明细或模型定价尚未同步，请稍后刷新。",
              )
            : t(
                "usage.cycleCapacity.empty.localDescription",
                "额度比例已经可用，但当前周期的本地 Token 或成本尚不足以进行估算。",
              );
  const waitingMetricValue = t(
    "usage.cycleCapacity.empty.metricValue",
    "待估算",
  );
  const ringDetailLabel = estimate
    ? estimateLabel
    : t("usage.cycleCapacity.empty.ringLabel", "等待可估算用量");
  const normalizedEmail = accountEmail?.trim() || null;
  const refreshTimestamp =
    lastRefreshedAt != null && Number.isFinite(lastRefreshedAt)
      ? lastRefreshedAt
      : null;
  const lastRefreshLabel = refreshTimestamp
    ? formatCycleDateTime(refreshTimestamp, locale)
    : null;

  const handleExpandedChange = (expanded: boolean) => {
    setIsExpanded(expanded);
    persistExpandedState(expanded);
  };

  const handleModeChange = (mode: CodexCycleCapacityCalculationMode) => {
    if (calculationMode == null) setUncontrolledMode(mode);
    persistCodexCycleCapacityMode(mode);
    onCalculationModeChange?.(mode);
  };

  const formatUsdEstimate = (value: number) =>
    `${fmtUsd(value, 2)}${activeAnalyticsEstimate?.hasUnknownPricing ? "*" : ""}`;

  return (
    <Collapsible asChild open={isExpanded} onOpenChange={handleExpandedChange}>
      <Card
        className={cn(
          "overflow-hidden border border-border/50 bg-card/60 shadow-sm backdrop-blur-xl",
          className,
        )}
        data-testid="codex-cycle-capacity-card"
      >
        <CardContent className="p-4 md:p-5">
          <div className="flex flex-col gap-2.5 sm:flex-row sm:items-center sm:justify-between">
            <div className="flex min-w-0 items-center gap-3">
              <div className="rounded-lg bg-emerald-500/10 p-2 text-emerald-600 dark:text-emerald-400">
                <Gauge className="h-5 w-5" />
              </div>
              <div className="min-w-0 flex-1">
                <div
                  className="flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-0.5"
                  data-testid="codex-capacity-identity-line"
                >
                  <h3 className="shrink-0 text-base font-semibold tracking-tight">
                    {titleLabel}
                  </h3>
                  {normalizedEmail ? (
                    <div className="flex min-w-0 items-center gap-1.5 border-l border-border/60 pl-2.5 text-[11px] text-muted-foreground">
                      <Mail className="h-3.5 w-3.5 shrink-0" />
                      <span className="sr-only">
                        {t("usage.cycleCapacity.accountEmail", "邮箱")}:
                      </span>
                      <span
                        className="min-w-0 truncate text-foreground/75"
                        title={normalizedEmail}
                      >
                        {normalizedEmail}
                      </span>
                    </div>
                  ) : null}
                </div>
                <div
                  className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2.5 gap-y-0.5 text-[11px] text-muted-foreground"
                  data-testid="codex-capacity-timing-line"
                >
                  <div className="flex min-w-0 items-center gap-1.5">
                    <CalendarClock className="h-3.5 w-3.5 shrink-0" />
                    <span className="truncate tabular-nums" title={cycleRange}>
                      {cycleRange}
                    </span>
                  </div>
                  {lastRefreshLabel ? (
                    <div className="flex min-w-0 items-center gap-1.5 border-l border-border/60 pl-2.5">
                      <Clock3 className="h-3.5 w-3.5 shrink-0" />
                      <span className="shrink-0">
                        {t("usage.cycleCapacity.lastRefresh", "上次刷新")}
                      </span>
                      <span
                        className="truncate tabular-nums text-foreground/75"
                        title={lastRefreshLabel}
                      >
                        {lastRefreshLabel}
                      </span>
                    </div>
                  ) : null}
                </div>
              </div>
            </div>

            <div className="flex shrink-0 flex-wrap items-center gap-1.5 sm:justify-end">
              <div
                className="flex min-w-0 flex-1 items-center gap-1.5 rounded-lg border border-border/40 bg-background/40 px-2.5 py-1.5 text-[10px] leading-tight text-muted-foreground sm:flex-none"
                aria-label={basisLabel}
                title={basisLabel}
              >
                <Info className="h-3.5 w-3.5 shrink-0" />
                {compactBasisLabel}
              </div>
              {showModeSwitch ? (
                <div
                  className="inline-flex rounded-lg border border-border/60 bg-muted/45 p-0.5"
                  role="radiogroup"
                  aria-label={t(
                    "usage.cycleCapacity.calculationMode",
                    "估算方式",
                  )}
                >
                  {(
                    [
                      [
                        "local",
                        t("usage.cycleCapacity.localCalculation", "本地日志"),
                      ],
                      [
                        "analytics",
                        t(
                          "usage.cycleCapacity.analyticsCalculation",
                          "官方接口",
                        ),
                      ],
                    ] as const
                  ).map(([mode, label]) => (
                    <Button
                      key={mode}
                      type="button"
                      variant="ghost"
                      size="sm"
                      role="radio"
                      aria-checked={effectiveMode === mode}
                      className={cn(
                        "h-7 rounded-md px-3 text-[11px] shadow-none transition-none",
                        effectiveMode === mode
                          ? "bg-background font-semibold text-foreground shadow-sm hover:bg-background"
                          : "text-muted-foreground hover:text-foreground",
                      )}
                      onClick={() => handleModeChange(mode)}
                    >
                      {label}
                    </Button>
                  ))}
                </div>
              ) : null}
              {selectedMode === "analytics" && onRefreshAnalytics ? (
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  className="h-8 shrink-0 gap-1.5 px-2.5 text-[11px]"
                  disabled={isRefreshingAnalytics}
                  aria-label={t("common.refresh", "刷新")}
                  title={t("common.refresh", "刷新")}
                  onClick={() => void onRefreshAnalytics()}
                >
                  <RefreshCw
                    className={cn(
                      "h-3.5 w-3.5",
                      isRefreshingAnalytics && "animate-spin",
                    )}
                    aria-hidden="true"
                  />
                  <span>
                    {isRefreshingAnalytics
                      ? t("common.refreshing", "刷新中...")
                      : t("common.refresh", "刷新")}
                  </span>
                </Button>
              ) : null}
              <CollapsibleTrigger asChild>
                <Button
                  type="button"
                  variant="ghost"
                  size="sm"
                  className="h-9 min-w-9 shrink-0 gap-1.5 px-2.5 transition-[background-color,color,transform] duration-150 active:scale-[0.97]"
                  aria-label={toggleAriaLabel}
                  title={toggleAriaLabel}
                  data-testid="codex-cycle-capacity-toggle"
                >
                  <span className="hidden sm:inline">{toggleLabel}</span>
                  <ChevronDown
                    className={cn(
                      "h-4 w-4 transition-transform duration-200 motion-reduce:transition-none",
                      isExpanded && "rotate-180",
                    )}
                    aria-hidden="true"
                  />
                </Button>
              </CollapsibleTrigger>
            </div>
          </div>

          <CollapsibleContent>
            <div className="mt-4 grid gap-5 lg:grid-cols-[190px_minmax(0,1fr)] lg:items-center">
              <CapacityRing
                utilizationPercent={utilizationPercent}
                gradientId={gradientId}
                usedLabel={usedLabel}
                detailLabel={ringDetailLabel}
              />

              <div className="min-w-0">
                <div className="mb-4 h-2 overflow-hidden rounded-full bg-muted/70">
                  <div
                    className="h-full rounded-full bg-gradient-to-r from-emerald-500 to-blue-500"
                    style={{
                      width: `${Math.min(100, utilizationPercent ?? 0)}%`,
                    }}
                  />
                </div>

                {!estimate ? (
                  <div
                    className="mb-3 flex items-start gap-2.5 rounded-xl border border-border/50 bg-muted/25 px-3.5 py-3"
                    role="status"
                    data-testid="codex-capacity-waiting-state"
                    data-reason={waitingReason}
                  >
                    <Clock3 className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
                    <div className="min-w-0">
                      <div className="text-sm font-semibold text-foreground">
                        {waitingTitle}
                      </div>
                      <div className="mt-0.5 text-[11px] leading-relaxed text-muted-foreground">
                        {waitingDescription}
                      </div>
                    </div>
                  </div>
                ) : null}

                <div
                  className="grid grid-cols-1 gap-2.5 sm:grid-cols-2 xl:grid-cols-3"
                  data-testid="codex-capacity-metrics"
                >
                  <CapacityMetric
                    label={t(
                      "usage.cycleCapacity.totalTokens",
                      "完整周期 Token 等效容量",
                    )}
                    value={
                      estimate
                        ? formatEstimatedTokens(estimate.totalTokens)
                        : waitingMetricValue
                    }
                    title={
                      estimate
                        ? Math.round(estimate.totalTokens).toLocaleString(
                            locale,
                          )
                        : waitingDescription
                    }
                    emphasized
                  />
                  <CapacityMetric
                    label={t(
                      "usage.cycleCapacity.usedTokens",
                      "已用额度 Token 等效容量",
                    )}
                    value={
                      estimate
                        ? formatEstimatedTokens(estimate.usedTokens)
                        : waitingMetricValue
                    }
                    title={
                      estimate
                        ? Math.round(estimate.usedTokens).toLocaleString(locale)
                        : waitingDescription
                    }
                  />
                  <CapacityMetric
                    label={t(
                      "usage.cycleCapacity.remainingTokens",
                      "剩余额度 Token 等效容量",
                    )}
                    value={
                      estimate
                        ? formatEstimatedTokens(estimate.remainingTokens)
                        : waitingMetricValue
                    }
                    title={
                      estimate
                        ? Math.round(estimate.remainingTokens).toLocaleString(
                            locale,
                          )
                        : waitingDescription
                    }
                  />
                  <CapacityMetric
                    label={t(
                      "usage.cycleCapacity.totalUsd",
                      "完整周期美元等效容量",
                    )}
                    value={
                      estimate
                        ? formatUsdEstimate(estimate.totalUsd)
                        : waitingMetricValue
                    }
                    title={estimate ? undefined : waitingDescription}
                    emphasized
                  />
                  <CapacityMetric
                    label={t(
                      "usage.cycleCapacity.usedUsd",
                      "已用额度美元等效容量",
                    )}
                    value={
                      estimate
                        ? formatUsdEstimate(estimate.usedUsd)
                        : waitingMetricValue
                    }
                    title={estimate ? undefined : waitingDescription}
                  />
                  <CapacityMetric
                    label={t(
                      "usage.cycleCapacity.remainingUsd",
                      "剩余额度美元等效容量",
                    )}
                    value={
                      estimate
                        ? formatUsdEstimate(estimate.remainingUsd)
                        : waitingMetricValue
                    }
                    title={estimate ? undefined : waitingDescription}
                  />
                </div>
              </div>
            </div>

            {cycleWindow ? (
              <CodexCycleForecastPanel
                forecast={forecast}
                cycleStartMs={cycleWindow.startMs}
                queriedAtMs={cycleWindow.endMs}
                resetAtMs={cycleWindow.resetAtMs}
                waitingTitle={waitingTitle}
                waitingDescription={waitingDescription}
                locale={locale}
                lang={lang}
                t={t}
              />
            ) : null}

            <div className="mt-4 flex items-start gap-2 rounded-lg bg-muted/30 px-3 py-2 text-[10px] leading-relaxed text-muted-foreground">
              <CircleDollarSign className="mt-0.5 h-3.5 w-3.5 shrink-0" />
              <span>
                {disclaimer}
                {activeAnalyticsEstimate?.hasUnknownPricing
                  ? ` ${t(
                      "usage.cycleCapacity.unknownPricing",
                      "带 * 的金额只累计已识别公开价格的模型。",
                    )}`
                  : ""}
              </span>
            </div>
          </CollapsibleContent>
        </CardContent>
      </Card>
    </Collapsible>
  );
}
