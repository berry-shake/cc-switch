import { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { Input } from "@/components/ui/input";
import type { CodexQuotaCycleWindow } from "@/lib/codexCycleCapacity";
import {
  parseCreditsPerUsd,
  persistCreditsPerUsd,
  readCreditsPerUsd,
  summarizePersonalCredits,
} from "@/lib/codexPersonalCredits";
import type { CodexPersonalCredits } from "@/types/subscription";
import { formatCompactCount } from "./format";
import { CapacityMetric } from "./CapacityMetric";

export function CodexPersonalCreditsPanel({
  data,
  cycle,
}: {
  data: CodexPersonalCredits;
  cycle: CodexQuotaCycleWindow | null;
}) {
  const { t } = useTranslation();
  const id = useId();
  const [rateText, setRateText] = useState(() => String(readCreditsPerUsd()));
  const rate = parseCreditsPerUsd(rateText);
  const stats = summarizePersonalCredits(data, cycle, rate);
  const unknown = t("usage.personalCredits.unknown", "待同步 / 待估算");
  const num = (value: number | null | undefined) =>
    value == null
      ? unknown
      : value.toLocaleString("en-US", { maximumFractionDigits: 2 });
  const usd = (value: number | null | undefined) =>
    value == null
      ? unknown
      : value.toLocaleString("en-US", { style: "currency", currency: "USD" });
  const tokenCount = (value: number | null | undefined) =>
    value == null ? unknown : formatCompactCount(value);
  const partial = t(
    "usage.personalCredits.partial",
    "仅合计已知部分；缺失项不按零计算，暂停对应容量外推。",
  );
  const metrics = [
    {
      label: t("usage.cycleCapacity.totalTokens", "完整周期 Token 等效容量"),
      value: tokenCount(stats?.totalTokens),
      title: num(stats?.totalTokens),
      emphasized: true,
    },
    {
      label: t("usage.cycleCapacity.usedTokens", "已用额度 Token 等效容量"),
      value: tokenCount(stats?.tokens),
      title: num(stats?.tokens),
    },
    {
      label: t(
        "usage.cycleCapacity.remainingTokens",
        "剩余额度 Token 等效容量",
      ),
      value: tokenCount(stats?.remainingTokens),
      title: num(stats?.remainingTokens),
    },
    {
      label: t("usage.cycleCapacity.totalUsd", "完整周期美元等效容量"),
      value: usd(stats?.totalUsd),
      emphasized: true,
    },
    {
      label: t("usage.cycleCapacity.usedUsd", "已用额度美元等效容量"),
      value: usd(stats?.usedUsd),
    },
    {
      label: t("usage.cycleCapacity.remainingUsd", "剩余额度美元等效容量"),
      value: usd(stats?.remainingUsd),
    },
  ];

  return (
    <section
      className="min-w-0 space-y-3"
      data-testid="codex-personal-credits-panel"
    >
      {data.totalsStatus !== "available" ? (
        <p
          role="status"
          className="text-xs text-amber-600"
          data-testid="credits-totals-warning"
        >
          {data.totalsStatus === "invalid"
            ? t(
                "usage.personalCredits.invalidTotals",
                "日合计的单位、日期或结构校验失败，未将其作为原始 Credits 使用。",
              )
            : t(
                "usage.personalCredits.unavailableTotals",
                "日合计暂不可用；额度和消耗速度预测仍保留。",
              )}
        </p>
      ) : null}
      {stats &&
      (stats.missingCreditDates.length > 0 ||
        stats.missingTokenDates.length > 0) ? (
        <p role="status" className="text-xs text-amber-600">
          {partial}{" "}
          {[
            ...new Set([
              ...stats.missingCreditDates,
              ...stats.missingTokenDates,
            ]),
          ].join(", ")}
        </p>
      ) : null}
      {stats && stats.boundaryDates.length > 0 ? (
        <p role="status" className="text-xs text-amber-600">
          {t(
            "usage.personalCredits.boundary",
            "周期边界日计入整日数据，可能包含周期外消耗：",
          )}{" "}
          {stats.boundaryDates.join(", ")}
        </p>
      ) : null}
      <div
        className="grid grid-cols-1 gap-2.5 sm:grid-cols-2 xl:grid-cols-3"
        data-testid="codex-capacity-metrics"
      >
        {metrics.map((metric) => (
          <CapacityMetric key={metric.label} {...metric} />
        ))}
      </div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-2 text-[11px] text-muted-foreground">
        <span
          data-testid="codex-personal-credits-total"
          className="tabular-nums"
        >
          {t("usage.personalCredits.used", "已用 Credits（接口原始）")}:{" "}
          {num(stats?.credits)}
        </span>
        <div className="flex items-center gap-1.5">
          <label
            htmlFor={id}
            title={t(
              "usage.personalCredits.rate",
              "换算系数：每 1 USD 对应 Credits",
            )}
          >
            Credits / USD
          </label>
          <Input
            id={id}
            type="number"
            min="0.000001"
            max="1000000000"
            step="any"
            value={rateText}
            className="h-6 w-20 px-2 text-xs tabular-nums"
            aria-label={t(
              "usage.personalCredits.rate",
              "换算系数：每 1 USD 对应 Credits",
            )}
            aria-describedby={`${id}-hint`}
            aria-invalid={rate == null}
            onChange={(event) => setRateText(event.target.value)}
            onBlur={() => persistCreditsPerUsd(rateText)}
          />
        </div>
        <span id={`${id}-hint`}>
          {t("usage.personalCredits.rateHint", "自定义等值，非官方账单汇率。")}
        </span>
      </div>
      {rate == null ? (
        <p role="alert" className="text-xs text-amber-600">
          {t(
            "usage.personalCredits.invalidRate",
            "请输入大于 0 且不超过 1,000,000,000 的换算系数。",
          )}
        </p>
      ) : null}
    </section>
  );
}
