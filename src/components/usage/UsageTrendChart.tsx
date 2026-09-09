import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import {
  AreaChart,
  Area,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from "recharts";
import { useUsageTrends } from "@/lib/query/usage";
import { Loader2 } from "lucide-react";
import {
  fmtInt,
  fmtUsd,
  getLocaleFromLanguage,
  parseFiniteNumber,
} from "./format";
import { resolveUsageRange } from "@/lib/usageRange";
import type { UsageRangeSelection } from "@/types/usage";

interface UsageTrendChartProps {
  range: UsageRangeSelection;
  rangeLabel: string;
  appType?: string;
  providerName?: string;
  model?: string;
  refreshIntervalMs: number;
}

export interface UsageTrendStatLike {
  date: string;
  totalInputTokens: number;
  totalOutputTokens: number;
  totalCacheCreationTokens: number;
  totalCacheReadTokens: number;
  totalCost: string | number;
}

export interface UsageTrendChartPoint {
  /** Unique category key for Recharts — must not collide across years. */
  xKey: string;
  rawDate: string;
  /** Short tick label shown on the X axis. */
  label: string;
  /** Fuller label used by the tooltip. */
  tooltipLabel: string;
  hour: number;
  inputTokens: number;
  cacheCreationTokens: number;
  cacheReadTokens: number;
  outputTokens: number;
  cost: number | null;
}

type UsageTrendSeriesKey =
  | "inputTokens"
  | "cacheCreationTokens"
  | "cacheReadTokens"
  | "outputTokens"
  | "cost";

interface UsageTrendSeriesDefinition {
  dataKey: UsageTrendSeriesKey;
  nameKey: string;
  fallback: string;
  yAxisId: "tokens" | "cost";
  stroke: string;
  fill: string;
  fillOpacity?: number;
  strokeDasharray?: string;
}

/** Shared order for chart layers, legend entries, and tooltip rows. */
export const USAGE_TREND_SERIES: readonly UsageTrendSeriesDefinition[] = [
  {
    dataKey: "inputTokens",
    nameKey: "usage.inputTokens",
    fallback: "输入",
    yAxisId: "tokens",
    stroke: "#3b82f6",
    fill: "url(#colorInput)",
    fillOpacity: 1,
  },
  {
    dataKey: "cacheCreationTokens",
    nameKey: "usage.cacheCreationTokens",
    fallback: "缓存写入",
    yAxisId: "tokens",
    stroke: "#f97316",
    fill: "url(#colorCacheCreation)",
    fillOpacity: 1,
  },
  {
    dataKey: "cacheReadTokens",
    nameKey: "usage.cacheReadTokens",
    fallback: "缓存读取",
    yAxisId: "tokens",
    stroke: "#a855f7",
    fill: "url(#colorCacheRead)",
    fillOpacity: 1,
  },
  {
    dataKey: "outputTokens",
    nameKey: "usage.outputTokens",
    fallback: "输出",
    yAxisId: "tokens",
    stroke: "#22c55e",
    fill: "url(#colorOutput)",
    fillOpacity: 1,
  },
  {
    dataKey: "cost",
    nameKey: "usage.cost",
    fallback: "成本",
    yAxisId: "cost",
    stroke: "#f43f5e",
    fill: "none",
    strokeDasharray: "4 4",
  },
];

const usageTrendSeriesOrder = new Map(
  USAGE_TREND_SERIES.map((series, index) => [series.dataKey, index]),
);

/** Keep tooltip rows canonical even if Recharts changes payload order. */
export function orderUsageTrendTooltipPayload<T extends { dataKey?: unknown }>(
  payload: readonly T[],
): T[] {
  return payload
    .map((entry, index) => ({
      entry,
      index,
      order:
        usageTrendSeriesOrder.get(
          String(entry.dataKey) as UsageTrendSeriesKey,
        ) ?? USAGE_TREND_SERIES.length,
    }))
    .sort((a, b) => a.order - b.order || a.index - b.index)
    .map(({ entry }) => entry);
}

/** Build chart rows from backend trend stats. Exported for unit tests. */
export function buildUsageTrendChartData(
  trends: UsageTrendStatLike[] | undefined,
  options: {
    isHourly: boolean;
    dateLocale: string;
    /** Inclusive range endpoints (unix seconds). Used to decide year labels. */
    startDate: number;
    endDate: number;
  },
): UsageTrendChartPoint[] {
  const { isHourly, dateLocale, startDate, endDate } = options;
  const startYear = new Date(startDate * 1000).getFullYear();
  const endYear = new Date(endDate * 1000).getFullYear();
  const spansMultipleYears = startYear !== endYear;

  return (
    trends?.map((stat) => {
      const pointDate = new Date(stat.date);
      const cost = parseFiniteNumber(stat.totalCost);
      // Prefer a stable unique key from the source timestamp / date string.
      // Falling back to ISO keeps categories unique even if the backend
      // returns sparse points that share the same local MM/DD across years.
      const xKey = stat.date;
      const tooltipLabel = isHourly
        ? pointDate.toLocaleString(dateLocale, {
            year: "numeric",
            month: "2-digit",
            day: "2-digit",
            hour: "2-digit",
            minute: "2-digit",
          })
        : pointDate.toLocaleDateString(dateLocale, {
            year: "numeric",
            month: "2-digit",
            day: "2-digit",
          });
      const label = isHourly
        ? pointDate.toLocaleString(dateLocale, {
            month: "2-digit",
            day: "2-digit",
            hour: "2-digit",
            minute: "2-digit",
          })
        : spansMultipleYears
          ? pointDate.toLocaleDateString(dateLocale, {
              year: "2-digit",
              month: "2-digit",
              day: "2-digit",
            })
          : pointDate.toLocaleDateString(dateLocale, {
              month: "2-digit",
              day: "2-digit",
            });

      return {
        xKey,
        rawDate: stat.date,
        label,
        tooltipLabel,
        hour: pointDate.getHours(),
        inputTokens: stat.totalInputTokens,
        cacheCreationTokens: stat.totalCacheCreationTokens,
        cacheReadTokens: stat.totalCacheReadTokens,
        outputTokens: stat.totalOutputTokens,
        cost: cost ?? null,
      };
    }) || []
  );
}

/** Resolve a tick label by the unique category key (not by filtered tick index). */
export function formatUsageTrendTickLabel(
  xKey: string,
  chartData: UsageTrendChartPoint[],
): string {
  const point = chartData.find((row) => row.xKey === xKey);
  return point?.label ?? xKey;
}

export function createUsageTrendTokenTickFormatter(
  locale: string,
): Intl.NumberFormat {
  return new Intl.NumberFormat(locale, {
    notation: "compact",
    compactDisplay: "short",
    maximumFractionDigits: 1,
  });
}

export function formatUsageTrendTokenTickLabel(
  value: unknown,
  formatter: Intl.NumberFormat,
): string {
  const num = parseFiniteNumber(value);
  if (num == null) return "--";

  return formatter.format(num);
}

export function UsageTrendChart({
  range,
  rangeLabel,
  appType,
  providerName,
  model,
  refreshIntervalMs,
}: UsageTrendChartProps) {
  const { t, i18n } = useTranslation();
  const { startDate, endDate } = resolveUsageRange(range);
  const { data: trends, isLoading } = useUsageTrends(
    range,
    { appType, providerName, model },
    {
      refetchInterval: refreshIntervalMs > 0 ? refreshIntervalMs : false,
    },
  );

  const durationSeconds = Math.max(endDate - startDate, 0);
  const isHourly = durationSeconds <= 24 * 60 * 60;
  const language = i18n.resolvedLanguage || i18n.language || "en";
  const dateLocale = getLocaleFromLanguage(language);
  const tokenTickFormatter = useMemo(
    () => createUsageTrendTokenTickFormatter(dateLocale),
    [dateLocale],
  );

  const chartData = useMemo(
    () =>
      buildUsageTrendChartData(trends, {
        isHourly,
        dateLocale,
        startDate,
        endDate,
      }),
    [trends, isHourly, dateLocale, startDate, endDate],
  );

  if (isLoading) {
    return (
      <div className="flex h-[350px] items-center justify-center rounded-xl bg-card/40 border border-border/50">
        <Loader2 className="h-8 w-8 animate-spin text-muted-foreground/30" />
      </div>
    );
  }

  const CustomTooltip = ({ active, payload }: any) => {
    if (active && payload && payload.length) {
      const point = payload[0]?.payload as UsageTrendChartPoint | undefined;
      const heading = point?.tooltipLabel ?? point?.label ?? "";
      const orderedPayload = orderUsageTrendTooltipPayload(payload);
      return (
        <div className="rounded-lg border bg-background/95 p-3 shadow-lg backdrop-blur-md">
          <p className="mb-2 font-medium">{heading}</p>
          {orderedPayload.map((entry: any, index: number) => (
            <div
              key={`${String(entry.dataKey)}-${index}`}
              className="flex items-center gap-2 text-sm"
              style={{ color: entry.color }}
            >
              <div
                className="h-2 w-2 rounded-full"
                style={{ backgroundColor: entry.color }}
              />
              <span className="font-medium">{entry.name}:</span>
              <span>
                {entry.dataKey === "cost"
                  ? fmtUsd(entry.value, 6)
                  : fmtInt(entry.value, dateLocale)}
              </span>
            </div>
          ))}
        </div>
      );
    }
    return null;
  };

  return (
    <div className="rounded-xl border border-border/50 bg-card/40 p-6 backdrop-blur-sm">
      <div className="mb-6 flex items-center justify-between">
        <h3 className="text-lg font-semibold">
          {t("usage.trends", "使用趋势")}
        </h3>
        <p className="text-sm text-muted-foreground">{rangeLabel}</p>
      </div>

      <div className="h-[350px] w-full">
        <ResponsiveContainer width="100%" height="100%">
          <AreaChart
            data={chartData}
            margin={{ top: 10, right: 10, left: 0, bottom: 0 }}
          >
            <defs>
              <linearGradient id="colorInput" x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor="#3b82f6" stopOpacity={0.2} />
                <stop offset="95%" stopColor="#3b82f6" stopOpacity={0} />
              </linearGradient>
              <linearGradient id="colorOutput" x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor="#22c55e" stopOpacity={0.2} />
                <stop offset="95%" stopColor="#22c55e" stopOpacity={0} />
              </linearGradient>
              <linearGradient
                id="colorCacheCreation"
                x1="0"
                y1="0"
                x2="0"
                y2="1"
              >
                <stop offset="5%" stopColor="#f97316" stopOpacity={0.2} />
                <stop offset="95%" stopColor="#f97316" stopOpacity={0} />
              </linearGradient>
              <linearGradient id="colorCacheRead" x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor="#a855f7" stopOpacity={0.2} />
                <stop offset="95%" stopColor="#a855f7" stopOpacity={0} />
              </linearGradient>
            </defs>
            <CartesianGrid
              strokeDasharray="3 3"
              vertical={false}
              stroke="hsl(var(--border))"
              opacity={0.4}
            />
            <XAxis
              dataKey="xKey"
              axisLine={false}
              tickLine={false}
              tick={{ fill: "hsl(var(--muted-foreground))", fontSize: 12 }}
              dy={10}
              tickFormatter={(value) =>
                formatUsageTrendTickLabel(String(value), chartData)
              }
              allowDuplicatedCategory={false}
            />
            <YAxis
              yAxisId="tokens"
              width={72}
              axisLine={false}
              tickLine={false}
              tickMargin={8}
              tick={{ fill: "hsl(var(--muted-foreground))", fontSize: 12 }}
              tickFormatter={(value) =>
                formatUsageTrendTokenTickLabel(value, tokenTickFormatter)
              }
            />
            <YAxis
              yAxisId="cost"
              orientation="right"
              width={56}
              axisLine={false}
              tickLine={false}
              tickMargin={8}
              tick={{ fill: "hsl(var(--muted-foreground))", fontSize: 12 }}
              tickFormatter={(value) => `$${value}`}
            />
            <Tooltip content={<CustomTooltip />} />
            <Legend />
            {USAGE_TREND_SERIES.map((series) => (
              <Area
                key={series.dataKey}
                yAxisId={series.yAxisId}
                type="monotone"
                dataKey={series.dataKey}
                name={t(series.nameKey, series.fallback)}
                stroke={series.stroke}
                fill={series.fill}
                fillOpacity={series.fillOpacity}
                strokeWidth={2}
                strokeDasharray={series.strokeDasharray}
              />
            ))}
          </AreaChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
