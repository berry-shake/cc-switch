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
    [
      t("usage.personalCredits.used", "已用 Credits（接口原始）"),
      num(stats?.credits),
    ],
    [
      t("usage.personalCredits.total", "100% 周期 Credits 容量（估算）"),
      num(stats?.totalCredits),
    ],
    [
      t("usage.personalCredits.remaining", "剩余 Credits 容量（估算）"),
      num(stats?.remainingCredits),
    ],
    [
      t("usage.personalCredits.usedUsd", "已用美元等值（估算）"),
      usd(stats?.usedUsd),
    ],
    [
      t("usage.personalCredits.totalUsd", "100% 周期美元等值（估算）"),
      usd(stats?.totalUsd),
    ],
    [
      t("usage.personalCredits.remainingUsd", "剩余美元等值（估算）"),
      usd(stats?.remainingUsd),
    ],
    [
      t("usage.personalCredits.tokens", "已同步 Token（日桶合计）"),
      tokenCount(stats?.tokens),
    ],
    [
      t("usage.personalCredits.totalTokens", "100% 周期 Token 容量（估算）"),
      tokenCount(stats?.totalTokens),
    ],
    [
      t("usage.personalCredits.remainingTokens", "剩余 Token 容量（估算）"),
      tokenCount(stats?.remainingTokens),
    ],
  ];

  return (
    <section
      className="min-w-0 space-y-3"
      data-testid="codex-personal-credits-panel"
    >
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <label htmlFor={id}>
          {t("usage.personalCredits.rate", "换算系数：每 1 USD 对应 Credits")}
        </label>
        <Input
          id={id}
          type="number"
          min="0.000001"
          max="1000000000"
          step="any"
          value={rateText}
          className="h-8 w-28 tabular-nums"
          aria-invalid={rate == null}
          onChange={(event) => setRateText(event.target.value)}
          onBlur={() => persistCreditsPerUsd(rateText)}
        />
        <span className="text-muted-foreground">
          {t(
            "usage.personalCredits.rateHint",
            "自定义等值换算，非官方账单汇率；不改变原始 Credits。",
          )}
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
      {data.breakdownStatus !== "available" ? (
        <p role="status" className="text-xs text-amber-600">
          {t(
            "usage.personalCredits.unavailableModels",
            "模型明细缺失或校验失败；已知日合计仍保留，无法确认的归属记为未分配。",
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
      <div className="grid grid-cols-1 gap-2.5 sm:grid-cols-2 xl:grid-cols-3">
        {metrics.map(([label, value]) => (
          <div
            key={label}
            className="min-w-0 rounded-xl border border-border/50 bg-background/45 p-3.5"
          >
            <div className="mb-1.5 text-[11px] font-medium text-muted-foreground">
              {label}
            </div>
            <div
              className="truncate text-lg font-bold tabular-nums"
              title={value}
            >
              {value}
            </div>
          </div>
        ))}
      </div>
      <details className="rounded-lg border border-border/50 p-3 text-xs">
        <summary className="cursor-pointer font-medium">
          {t("usage.personalCredits.details", "模型与每日 Credits 明细")}
        </summary>
        <div className="mt-3 space-y-3">
          <p className="text-muted-foreground">
            {t(
              "usage.personalCredits.allocationNote",
              "模型明细为接口绝对值或按同日权重还原；不反推模型 Token，不额外乘 Fast 倍率。",
            )}
          </p>
          <div className="max-h-64 overflow-auto">
            <table className="w-full text-left">
              <thead>
                <tr>
                  <th className="p-2">
                    {t("usage.personalCredits.model", "模型 / 速度")}
                  </th>
                  <th className="p-2 text-right">Credits</th>
                </tr>
              </thead>
              <tbody>
                {stats?.models.map((model) => (
                  <tr key={JSON.stringify([model.model, model.speed])}>
                    <td className="break-all p-2">
                      {model.model} · {model.speed}
                    </td>
                    <td className="p-2 text-right tabular-nums">
                      {num(model.credits)}
                    </td>
                  </tr>
                ))}
                <tr>
                  <td className="p-2">
                    {t("usage.personalCredits.unallocated", "未分配")}
                  </td>
                  <td className="p-2 text-right tabular-nums">
                    {num(stats?.unallocatedCredits)}
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          <div className="max-h-64 overflow-auto">
            <table className="w-full text-left">
              <thead>
                <tr>
                  <th className="p-2">
                    {t("usage.personalCredits.date", "日期")}
                  </th>
                  <th className="p-2 text-right">Credits</th>
                  <th className="p-2">
                    {t("usage.personalCredits.allocation", "模型归属")}
                  </th>
                </tr>
              </thead>
              <tbody>
                {stats?.days.map((day) => (
                  <tr key={day.date}>
                    <td className="whitespace-nowrap p-2">{day.date}</td>
                    <td className="p-2 text-right tabular-nums">
                      {num(day.credits)}
                    </td>
                    <td className="p-2">
                      {t(
                        `usage.personalCredits.status.${day.allocation}`,
                        {
                          reported: "接口原始值",
                          allocated: "按同日占比还原",
                          unallocated: "未分配",
                          mismatched: "明细不一致，未分配",
                          pending: "待同步",
                        }[day.allocation],
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      </details>
      <p className="text-[11px] leading-relaxed text-muted-foreground">
        {t(
          "usage.personalCredits.method",
          "Credits 保持接口原始值，未应用 Astra 缓存校正。100% 容量 = 已同步日合计 ÷ 官方已用比例；比例为零或数据缺失时不外推。日期桶按 UTC 对齐，接口未声明时区；日桶延迟和边界日会带来误差，不代表实际账单或期末必然消耗。",
        )}
      </p>
    </section>
  );
}
