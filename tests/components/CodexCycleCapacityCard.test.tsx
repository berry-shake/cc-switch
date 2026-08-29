import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY,
  CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
  CodexCycleCapacityCard,
} from "@/components/usage/CodexCycleCapacityCard";
import type { CodexQuotaSample } from "@/lib/codexQuotaSamples";
import type {
  CodexAnalyticsUsage,
  SubscriptionQuota,
} from "@/types/subscription";
import type { ModelPricing, UsageSummary } from "@/types/usage";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (
      key: string,
      fallbackOrOptions?:
        | string
        | ({ defaultValue?: string } & Record<string, unknown>),
    ) => {
      if (typeof fallbackOrOptions === "string") return fallbackOrOptions;
      const template = fallbackOrOptions?.defaultValue ?? key;
      return template.replace(/{{(\w+)}}/g, (_match, name: string) =>
        String(fallbackOrOptions?.[name] ?? ""),
      );
    },
    i18n: { resolvedLanguage: "en", language: "en" },
  }),
}));

const queriedAt = Date.UTC(2026, 7, 22, 8, 0, 0);
const resetAt = Date.UTC(2026, 7, 25, 8, 0, 0);

const quota: SubscriptionQuota = {
  tool: "codex",
  credentialStatus: "valid",
  credentialMessage: null,
  success: true,
  tiers: [
    {
      name: "seven_day",
      windowSeconds: 7 * 24 * 60 * 60,
      utilization: 30,
      resetsAt: new Date(resetAt).toISOString(),
    },
  ],
  extraUsage: null,
  error: null,
  queriedAt,
};

const usage: UsageSummary = {
  totalRequests: 42,
  totalCost: "691.48",
  totalInputTokens: 400_000_000,
  totalOutputTokens: 100_000_000,
  totalCacheCreationTokens: 15_000_000,
  totalCacheReadTokens: 250_000_000,
  successRate: 100,
  realTotalTokens: 765_000_000,
  cacheHitRate: 0.5,
};

const analyticsUsage: CodexAnalyticsUsage = {
  accountMode: "workspace",
  queriedAt,
  days: [
    {
      date: "2026-08-22",
      missingTokenData: false,
      missingModelBreakdown: false,
      totals: {
        uncachedInputTokens: 1_000_000,
        cachedInputTokens: 0,
        cacheWriteInputTokens: 0,
        outputTokens: 0,
        totalTokens: 1_000_000,
      },
      models: [
        {
          model: "gpt-5.6-sol",
          speed: "standard",
          credits: 0,
          tokens: {
            uncachedInputTokens: 1_000_000,
            cachedInputTokens: 0,
            cacheWriteInputTokens: 0,
            outputTokens: 0,
            totalTokens: 1_000_000,
          },
        },
      ],
    },
  ],
};

const personalAnalyticsWithGaps: CodexAnalyticsUsage = {
  accountMode: "personal",
  queriedAt,
  days: [
    {
      date: "2026-08-18",
      missingTokenData: false,
      missingModelBreakdown: false,
      totals: {
        uncachedInputTokens: 1_000_000,
        cachedInputTokens: 0,
        cacheWriteInputTokens: 0,
        outputTokens: 0,
        totalTokens: 1_000_000,
      },
      models: [
        {
          model: "gpt-5.6-sol",
          speed: "standard",
          credits: 1,
          tokens: {
            uncachedInputTokens: 0,
            cachedInputTokens: 0,
            cacheWriteInputTokens: 0,
            outputTokens: 0,
            totalTokens: 0,
          },
        },
      ],
    },
    {
      date: "2026-08-19",
      missingTokenData: true,
      missingModelBreakdown: false,
      totals: {
        uncachedInputTokens: 0,
        cachedInputTokens: 0,
        cacheWriteInputTokens: 0,
        outputTokens: 0,
        totalTokens: 0,
      },
      models: [
        {
          model: "gpt-5.6-sol",
          speed: "standard",
          credits: 1,
          tokens: {
            uncachedInputTokens: 0,
            cachedInputTokens: 0,
            cacheWriteInputTokens: 0,
            outputTokens: 0,
            totalTokens: 0,
          },
        },
      ],
    },
    {
      date: "2026-08-20",
      missingTokenData: false,
      missingModelBreakdown: true,
      totals: {
        uncachedInputTokens: 500_000,
        cachedInputTokens: 0,
        cacheWriteInputTokens: 0,
        outputTokens: 0,
        totalTokens: 500_000,
      },
      models: [],
    },
  ],
};

const modelPricing: ModelPricing[] = [
  {
    modelId: "gpt-5.6-sol",
    displayName: "GPT-5.6 Sol",
    inputCostPerMillion: "4",
    outputCostPerMillion: "20",
    cacheReadCostPerMillion: "0.4",
    cacheCreationCostPerMillion: "5",
  },
];

describe("CodexCycleCapacityCard", () => {
  beforeEach(() => {
    window.localStorage.removeItem(CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY);
    window.localStorage.removeItem(CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY);
  });

  it("keeps the existing card and switches between persisted data sources", () => {
    const { unmount } = render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={usage}
        analyticsUsage={analyticsUsage}
        modelPricing={modelPricing}
        nowMs={queriedAt}
      />,
    );

    expect(screen.getByTestId("codex-cycle-capacity-card")).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "本地日志" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(screen.getByText("模型 · 速度 · Token 结构")).toHaveAttribute(
      "title",
      "按当前模型、速度及 Token 结构折算",
    );
    expect(screen.getByText("$691.48")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("radio", { name: "官方接口" }));

    expect(screen.getByRole("radio", { name: "官方接口" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(screen.getByText("官方模型 Token · 速度")).toHaveAttribute(
      "aria-label",
      "按官方用量接口的模型级 Token 与速度折算",
    );
    expect(screen.getByText("$4.00")).toBeInTheDocument();
    expect(screen.getByText("$13.33")).toBeInTheDocument();
    expect(
      window.localStorage.getItem(CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY),
    ).toBe("analytics");

    unmount();
    render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={usage}
        analyticsUsage={analyticsUsage}
        modelPricing={modelPricing}
        nowMs={queriedAt}
      />,
    );

    expect(screen.getByRole("radio", { name: "官方接口" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(screen.getByText("$4.00")).toBeInTheDocument();
  });

  it("shows manual refresh only in official API mode", () => {
    const refreshAnalytics = vi.fn();
    const { rerender } = render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={usage}
        analyticsUsage={analyticsUsage}
        modelPricing={modelPricing}
        nowMs={queriedAt}
        onRefreshAnalytics={refreshAnalytics}
      />,
    );

    expect(
      screen.queryByRole("button", { name: "刷新" }),
    ).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("radio", { name: "官方接口" }));
    fireEvent.click(screen.getByRole("button", { name: "刷新" }));
    expect(refreshAnalytics).toHaveBeenCalledTimes(1);

    rerender(
      <CodexCycleCapacityCard
        quota={quota}
        usage={usage}
        analyticsUsage={analyticsUsage}
        modelPricing={modelPricing}
        nowMs={queriedAt}
        calculationMode="analytics"
        onRefreshAnalytics={refreshAnalytics}
        isRefreshingAnalytics
      />,
    );

    expect(screen.getByRole("button", { name: "刷新" })).toBeDisabled();
    expect(screen.getByText("刷新中...")).toBeInTheDocument();
  });

  it("surfaces delayed daily sources and the partial cycle start day", () => {
    render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={usage}
        analyticsUsage={personalAnalyticsWithGaps}
        modelPricing={modelPricing}
        calculationMode="analytics"
        onCalculationModeChange={vi.fn()}
        nowMs={queriedAt}
      />,
    );

    const quality = screen.getByTestId("codex-analytics-data-quality");
    expect(
      within(quality).getByTestId("codex-analytics-missing-token-notice"),
    ).toHaveTextContent(
      "以下日期已有模型额度记录，但缺少总 Token 日报：2026-08-19",
    );
    expect(
      within(quality).getByTestId("codex-analytics-missing-model-notice"),
    ).toHaveTextContent(
      "以下日期已有总 Token 日报，但缺少模型/速度额度明细：2026-08-20",
    );
    expect(
      within(quality).getByTestId("codex-analytics-partial-start-notice"),
    ).toHaveTextContent(
      "当前周期从官方统计日 2026-08-18 中途开始，接口无法拆分该日重置前后的用量",
    );
    // 起始日占 2/3 的已统计 Token，其中 1/3 落在重置之前。
    expect(
      within(quality).getByTestId("codex-analytics-partial-start-notice"),
    ).toHaveTextContent("偏高约 22%");
    expect(screen.getByTestId("codex-cycle-forecast")).toBeInTheDocument();
  });

  it("hides analytics data notices when no capacity cycle is resolved", () => {
    render(
      <CodexCycleCapacityCard
        quota={{ ...quota, tiers: [] }}
        quotaWindows={[
          {
            usedPercent: null,
            windowSeconds: 7 * 24 * 60 * 60,
            resetsAt: new Date(resetAt).toISOString(),
          },
        ]}
        usage={usage}
        analyticsUsage={personalAnalyticsWithGaps}
        modelPricing={modelPricing}
        calculationMode="analytics"
        onCalculationModeChange={vi.fn()}
        nowMs={queriedAt}
      />,
    );

    // 额度比例缺失时根本不会外推容量，提示不能声称容量偏高或偏低。
    expect(screen.getByTestId("codex-capacity-waiting-state")).toHaveAttribute(
      "data-reason",
      "utilization",
    );
    expect(
      screen.queryByTestId("codex-analytics-data-quality"),
    ).not.toBeInTheDocument();
  });

  it("shows the account email and the timestamp of the same quota refresh", () => {
    render(
      <CodexCycleCapacityCard
        quota={quota}
        accountEmail=" current@example.com "
        lastRefreshedAt={queriedAt}
        usage={usage}
        nowMs={queriedAt}
      />,
    );

    const identityLine = screen.getByTestId("codex-capacity-identity-line");
    expect(within(identityLine).getByText("邮箱:")).toHaveClass("sr-only");
    expect(
      within(identityLine).getByText("current@example.com"),
    ).toHaveAttribute("title", "current@example.com");
    const timingLine = screen.getByTestId("codex-capacity-timing-line");
    expect(within(timingLine).getByText("上次刷新")).toBeInTheDocument();
    const formattedRefresh = new Intl.DateTimeFormat("en-US", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
    }).format(new Date(queriedAt));
    expect(within(timingLine).getByText(formattedRefresh)).toBeInTheDocument();
  });

  it("defaults to expanded and restores the persisted collapse state", () => {
    const { unmount } = render(
      <CodexCycleCapacityCard quota={quota} usage={usage} nowMs={queriedAt} />,
    );

    const collapseButton = screen.getByRole("button", {
      name: "收起 Codex 周期等效容量（估算）",
    });
    expect(collapseButton).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("codex-cycle-forecast")).toBeInTheDocument();

    fireEvent.click(collapseButton);

    expect(
      screen.getByRole("button", {
        name: "展开 Codex 周期等效容量（估算）",
      }),
    ).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByTestId("codex-cycle-forecast"),
    ).not.toBeInTheDocument();
    expect(
      window.localStorage.getItem(CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY),
    ).toBe("false");

    unmount();
    render(
      <CodexCycleCapacityCard quota={quota} usage={usage} nowMs={queriedAt} />,
    );

    const expandButton = screen.getByRole("button", {
      name: "展开 Codex 周期等效容量（估算）",
    });
    expect(expandButton).toHaveAttribute("aria-expanded", "false");
    expect(
      screen.queryByTestId("codex-cycle-forecast"),
    ).not.toBeInTheDocument();

    fireEvent.click(expandButton);

    expect(
      screen.getByRole("button", {
        name: "收起 Codex 周期等效容量（估算）",
      }),
    ).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("codex-cycle-forecast")).toBeInTheDocument();
    expect(
      window.localStorage.getItem(CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY),
    ).toBe("true");
  });

  it("groups full, used, and remaining Token/USD capacity metrics in order", () => {
    render(
      <CodexCycleCapacityCard quota={quota} usage={usage} nowMs={queriedAt} />,
    );

    expect(screen.getByRole("progressbar", { name: "已用" })).toHaveAttribute(
      "aria-valuenow",
      "30",
    );
    expect(screen.getByText("30%")).toBeInTheDocument();
    const metrics = screen.getByTestId("codex-capacity-metrics");
    const metricLabels = [...metrics.children].map(
      (metric) => metric.firstElementChild?.textContent,
    );
    const metricValues = [...metrics.children].map(
      (metric) => metric.lastElementChild?.textContent,
    );
    expect(metricLabels).toEqual([
      "完整周期 Token 等效容量",
      "已用额度 Token 等效容量",
      "剩余额度 Token 等效容量",
      "完整周期美元等效容量",
      "已用额度美元等效容量",
      "剩余额度美元等效容量",
    ]);
    expect(metricValues).toEqual([
      "2.55B",
      "765.00M",
      "1.79B",
      "$2304.93",
      "$691.48",
      "$1613.45",
    ]);
    for (const label of metricLabels) {
      expect(label).not.toContain("估算");
    }

    const exactTokenTitles = ["2,550,000,000", "765,000,000", "1,785,000,000"];
    expect(
      [...metrics.children]
        .slice(0, 3)
        .map((metric) => metric.lastElementChild?.getAttribute("title")),
    ).toEqual(exactTokenTitles);

    expect(screen.getByTestId("codex-cycle-forecast")).toBeInTheDocument();
    expect(screen.getByText("当前状态")).toBeInTheDocument();
    expect(screen.getByText("低于匀速基准")).toHaveAttribute(
      "data-status",
      "below_pace",
    );
    expect(screen.getAllByText("采样中（至少需要 1 小时）")).toHaveLength(1);
    expect(screen.getByTestId("codex-cycle-recent-sampling")).toHaveAttribute(
      "role",
      "status",
    );
    expect(screen.getByText("本周期重置前不会耗尽")).toBeInTheDocument();

    const forecast = screen.getByTestId("codex-cycle-forecast");
    expect(forecast.querySelectorAll("[data-forecast-metric]")).toHaveLength(9);
    expect(
      screen
        .getByTestId("codex-cycle-forecast-overview")
        .querySelectorAll("[data-forecast-metric]"),
    ).toHaveLength(3);
    expect(
      screen
        .getByTestId("codex-cycle-forecast-models")
        .querySelectorAll("[data-forecast-metric]"),
    ).toHaveLength(4);
    expect(
      screen
        .getByTestId("codex-cycle-forecast-recent-model")
        .querySelectorAll("[data-forecast-metric]"),
    ).toHaveLength(2);
    expect(
      screen
        .getByTestId("codex-cycle-forecast-exhaustion-models")
        .querySelectorAll("[data-forecast-metric]"),
    ).toHaveLength(2);
    expect(
      [
        ...screen
          .getByTestId("codex-cycle-forecast-models")
          .querySelectorAll("[data-forecast-metric]"),
      ].map((metric) => metric.getAttribute("data-forecast-metric")),
    ).toEqual([
      "recent-rate",
      "projected-final-utilization",
      "cumulative-exhaustion-at",
      "recent-exhaustion-at",
    ]);
    expect(
      screen
        .getByTestId("codex-cycle-forecast-reset")
        .querySelectorAll("[data-forecast-metric]"),
    ).toHaveLength(2);
    expect(
      forecast.querySelector('[data-forecast-metric="actual-vs-baseline"]'),
    ).toHaveAttribute("data-tone", "info");
    expect(
      screen.getByRole("progressbar", {
        name: "实际用量 / 基准用量（同期）",
      }),
    ).toHaveAttribute("aria-valuenow", "30");
    expect(forecast.querySelector("[data-baseline-marker]")).not.toBeNull();
  });

  it("renders the reference pace and recent forecast from same-cycle samples", () => {
    const dayMs = 24 * 60 * 60 * 1000;
    const hourMs = 60 * 60 * 1000;
    const startMs = Date.UTC(2026, 7, 20, 12, 0, 0);
    const elapsedMs = dayMs + 23 * hourMs + 16 * 60 * 1000;
    const currentMs = startMs + elapsedMs;
    const cycleResetMs = startMs + 7 * dayMs;
    const recentSpanMs = 14 * hourMs + 48 * 60 * 1000;
    const referenceQuota: SubscriptionQuota = {
      ...quota,
      queriedAt: currentMs,
      tiers: [
        {
          name: "seven_day",
          windowSeconds: 7 * 24 * 60 * 60,
          utilization: 30,
          resetsAt: new Date(cycleResetMs).toISOString(),
        },
      ],
    };
    const samples: CodexQuotaSample[] = [
      {
        credentialScope: "scope-a",
        capturedAtMs: currentMs - recentSpanMs,
        cycleStartMs: startMs,
        resetAtMs: cycleResetMs,
        windowSeconds: 7 * 24 * 60 * 60,
        utilizationPercent: 15,
      },
    ];

    render(
      <CodexCycleCapacityCard
        quota={referenceQuota}
        usage={usage}
        quotaSamples={samples}
        nowMs={currentMs}
      />,
    );

    expect(screen.getByText("接近匀速基准")).toHaveAttribute(
      "data-status",
      "on_pace",
    );
    expect(
      screen.getByText("28.1% · 已过 1天 23小时 16分钟"),
    ).toBeInTheDocument();
    expect(screen.getByText("30% / 28.1%（基本一致）")).toBeInTheDocument();
    expect(screen.getByText("15.2%/天 / 14.3%/天")).toBeInTheDocument();
    expect(
      screen.getByText("24.3%/天 · 采用最近 14小时 48分钟 的采样"),
    ).toBeInTheDocument();
    expect(screen.getByText(/152\.4%（预计提前耗尽）/)).toBeInTheDocument();
    expect(
      screen
        .getByTestId("codex-cycle-forecast")
        .querySelector('[data-forecast-metric="actual-vs-baseline"]'),
    ).toHaveAttribute("data-tone", "success");
    expect(
      screen.queryByTestId("codex-cycle-recent-sampling"),
    ).not.toBeInTheDocument();

    const forecast = screen.getByTestId("codex-cycle-forecast");
    expect(forecast.querySelectorAll("dt")).toHaveLength(9);
    expect(forecast.querySelectorAll("dd")).toHaveLength(9);
  });

  it("does not label exhaustion exactly at reset as early", () => {
    const dayMs = 24 * 60 * 60 * 1000;
    const hourMs = 60 * 60 * 1000;
    const startMs = Date.UTC(2026, 7, 18, 8);
    const currentMs = startMs + 2 * dayMs;
    const cycleResetMs = startMs + 7 * dayMs;
    const sustainableRate = 100 / 7;
    const currentUtilization = sustainableRate * 2;
    const exactQuota: SubscriptionQuota = {
      ...quota,
      queriedAt: currentMs,
      tiers: [
        {
          name: "seven_day",
          windowSeconds: 7 * 24 * 60 * 60,
          utilization: currentUtilization,
          resetsAt: new Date(cycleResetMs).toISOString(),
        },
      ],
    };

    render(
      <CodexCycleCapacityCard
        quota={exactQuota}
        usage={usage}
        quotaSamples={[
          {
            credentialScope: "scope-a",
            capturedAtMs: currentMs - 2 * hourMs,
            cycleStartMs: startMs,
            resetAtMs: cycleResetMs,
            windowSeconds: 7 * 24 * 60 * 60,
            utilizationPercent: currentUtilization - sustainableRate * (2 / 24),
          },
        ]}
        nowMs={currentMs}
      />,
    );

    const label = screen.getByText("近期速率模型期末预计用量");
    expect(label.parentElement?.querySelector("dd")).toHaveTextContent("100%");
    expect(screen.queryByText(/预计提前耗尽/)).not.toBeInTheDocument();
  });

  it("binds the far-above status to the actual-versus-baseline summary", () => {
    const dayMs = 24 * 60 * 60 * 1000;
    const startMs = Date.UTC(2026, 7, 21, 16, 57);
    const currentMs = startMs + 19 * 60 * 60 * 1000 + 16 * 60 * 1000;
    const cycleResetMs = startMs + 7 * dayMs;
    const dangerQuota: SubscriptionQuota = {
      ...quota,
      queriedAt: currentMs,
      tiers: [
        {
          name: "seven_day",
          windowSeconds: 7 * 24 * 60 * 60,
          utilization: 35,
          resetsAt: new Date(cycleResetMs).toISOString(),
        },
      ],
    };

    render(
      <CodexCycleCapacityCard
        quota={dangerQuota}
        usage={usage}
        nowMs={currentMs}
      />,
    );

    expect(screen.getByText("明显高于匀速基准")).toHaveAttribute(
      "data-status",
      "far_above_pace",
    );
    const actualMetric = screen
      .getByTestId("codex-cycle-forecast")
      .querySelector('[data-forecast-metric="actual-vs-baseline"]');
    expect(actualMetric).toHaveAttribute("data-tone", "danger");
    expect(actualMetric).toHaveAttribute("data-prominence", "primary");
    expect(screen.getByText(/35% \/ 11\.5%（明显偏高）/)).toBeInTheDocument();
  });

  it("renders nothing when the quota request is unsuccessful", () => {
    const { container } = render(
      <CodexCycleCapacityCard
        quota={{ ...quota, success: false }}
        usage={usage}
        nowMs={queriedAt}
      />,
    );

    expect(container).toBeEmptyDOMElement();
  });

  it("keeps the card visible while local usage is not yet estimable", () => {
    render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={{ ...usage, totalCost: "0" }}
        nowMs={queriedAt}
      />,
    );

    expect(screen.getByTestId("codex-cycle-capacity-card")).toBeInTheDocument();
    expect(screen.getByTestId("codex-capacity-waiting-state")).toHaveAttribute(
      "data-reason",
      "local",
    );
    expect(screen.getByText("等待本地用量同步")).toBeInTheDocument();
    expect(screen.getAllByText("待估算")).toHaveLength(6);
    const forecast = screen.getByTestId("codex-cycle-forecast");
    expect(within(forecast).getByText("当前状态")).toBeInTheDocument();
    expect(within(forecast).getByText("低于匀速基准")).toHaveAttribute(
      "data-status",
      "below_pace",
    );
    expect(
      within(forecast).getByText("30% / 57.1%（低于基准）"),
    ).toBeInTheDocument();
    expect(within(forecast).getByText("周期计划重置时间")).toBeInTheDocument();
  });

  it.each([
    {
      missingBasis: "official Token usage",
      analyticsUsage: { ...analyticsUsage, days: [] },
      modelPricing,
    },
    {
      missingBasis: "model pricing",
      analyticsUsage,
      modelPricing: [] as ModelPricing[],
    },
  ])(
    "keeps the utilization forecast while waiting for $missingBasis",
    ({ analyticsUsage: pendingUsage, modelPricing: pendingPricing }) => {
      render(
        <CodexCycleCapacityCard
          quota={quota}
          usage={usage}
          analyticsUsage={pendingUsage}
          modelPricing={pendingPricing}
          calculationMode="analytics"
          onCalculationModeChange={vi.fn()}
          onRefreshAnalytics={vi.fn()}
          nowMs={queriedAt}
        />,
      );

      expect(
        screen.getByTestId("codex-capacity-waiting-state"),
      ).toHaveAttribute("data-reason", "analytics");
      expect(screen.getAllByText("待估算")).toHaveLength(6);

      const forecast = screen.getByTestId("codex-cycle-forecast");
      expect(within(forecast).getByText("当前状态")).toBeInTheDocument();
      expect(
        within(forecast).getByText("57.1% · 已过 4天"),
      ).toBeInTheDocument();
      expect(
        within(forecast).getByText("7.5%/天 / 14.3%/天"),
      ).toBeInTheDocument();
      expect(
        within(forecast).getByText("采样中（至少需要 1 小时）"),
      ).toBeInTheDocument();
    },
  );

  it("shows a real 0% cycle and keeps official refresh available", () => {
    const refreshAnalytics = vi.fn();
    const zeroQuota: SubscriptionQuota = {
      ...quota,
      tiers: [
        {
          ...quota.tiers[0],
          utilization: 0,
        },
      ],
    };

    render(
      <CodexCycleCapacityCard
        quota={zeroQuota}
        usage={null}
        calculationMode="analytics"
        onCalculationModeChange={vi.fn()}
        onRefreshAnalytics={refreshAnalytics}
        nowMs={queriedAt}
      />,
    );

    expect(screen.getByRole("progressbar", { name: "已用" })).toHaveAttribute(
      "aria-valuenow",
      "0",
    );
    expect(screen.getByText("0%")).toBeInTheDocument();
    expect(screen.getByTestId("codex-capacity-waiting-state")).toHaveAttribute(
      "data-reason",
      "firstUsage",
    );
    expect(screen.getByText("新周期已开始")).toBeInTheDocument();
    expect(
      screen.getByText(/首笔用量同步后，将自动计算 Token 和美元等效容量/),
    ).toBeInTheDocument();
    expect(screen.getAllByText("待估算")).toHaveLength(6);
    const forecast = screen.getByTestId("codex-cycle-forecast");
    expect(within(forecast).getByText("当前状态")).toBeInTheDocument();
    expect(within(forecast).getByText("低于匀速基准")).toHaveAttribute(
      "data-status",
      "below_pace",
    );
    expect(
      within(forecast).getByRole("progressbar", {
        name: "实际用量 / 基准用量（同期）",
      }),
    ).toHaveAttribute("aria-valuenow", "0");
    expect(
      within(forecast).getByText("0.0%/天 / 14.3%/天"),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "刷新" }));
    expect(refreshAnalytics).toHaveBeenCalledTimes(1);
  });

  it("distinguishes a missing used percentage from a real 0%", () => {
    render(
      <CodexCycleCapacityCard
        quota={{ ...quota, tiers: [] }}
        quotaWindows={[
          {
            usedPercent: null,
            windowSeconds: 7 * 24 * 60 * 60,
            resetsAt: new Date(resetAt).toISOString(),
          },
        ]}
        usage={null}
        calculationMode="analytics"
        onCalculationModeChange={vi.fn()}
        onRefreshAnalytics={vi.fn()}
        nowMs={queriedAt}
      />,
    );

    expect(
      screen.queryByRole("progressbar", { name: "已用" }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("status", { name: "等待可估算用量" }),
    ).toBeInTheDocument();
    expect(screen.getByTestId("codex-capacity-waiting-state")).toHaveAttribute(
      "data-reason",
      "utilization",
    );
    expect(
      within(screen.getByTestId("codex-capacity-waiting-state")).getByText(
        "等待额度比例",
      ),
    ).toBeInTheDocument();
    expect(
      within(screen.getByTestId("codex-capacity-waiting-state")).getByText(
        /不会把缺失值当成 0%/,
      ),
    ).toBeInTheDocument();

    const forecast = screen.getByTestId("codex-cycle-forecast");
    expect(within(forecast).getByText("当前状态")).toBeInTheDocument();
    expect(within(forecast).getByText("周期时间进度")).toBeInTheDocument();
    expect(within(forecast).getByText("周期计划重置时间")).toBeInTheDocument();
    expect(
      within(forecast).getByText("预测时点至重置的剩余时间"),
    ).toBeInTheDocument();
    expect(
      within(forecast).queryByRole("progressbar", {
        name: "实际用量 / 基准用量（同期）",
      }),
    ).not.toBeInTheDocument();
  });

  it("does not silently fall back to local data when official usage is empty", () => {
    const refreshAnalytics = vi.fn();
    render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={usage}
        analyticsUsage={{ ...analyticsUsage, days: [] }}
        modelPricing={modelPricing}
        calculationMode="analytics"
        onCalculationModeChange={vi.fn()}
        onRefreshAnalytics={refreshAnalytics}
        nowMs={queriedAt}
      />,
    );

    expect(screen.getByRole("radio", { name: "官方接口" })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    expect(screen.getByTestId("codex-capacity-waiting-state")).toHaveAttribute(
      "data-reason",
      "analytics",
    );
    expect(screen.getByText("等待官方用量同步")).toBeInTheDocument();
    expect(screen.queryByText("$691.48")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "刷新" }));
    expect(refreshAnalytics).toHaveBeenCalledTimes(1);
  });
});
