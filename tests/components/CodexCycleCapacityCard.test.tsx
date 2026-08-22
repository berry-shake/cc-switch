import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { CodexCycleCapacityCard } from "@/components/usage/CodexCycleCapacityCard";
import type { CodexQuotaSample } from "@/lib/codexQuotaSamples";
import type { SubscriptionQuota } from "@/types/subscription";
import type { UsageSummary } from "@/types/usage";

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

describe("CodexCycleCapacityCard", () => {
  it("renders the ratio and six explicitly estimated capacity metrics", () => {
    render(
      <CodexCycleCapacityCard quota={quota} usage={usage} nowMs={queriedAt} />,
    );

    expect(screen.getByRole("progressbar", { name: "已使用" })).toHaveAttribute(
      "aria-valuenow",
      "30",
    );
    expect(screen.getByText("30%")).toBeInTheDocument();
    expect(screen.getByText("70%")).toBeInTheDocument();
    expect(screen.getByText("2.55B")).toBeInTheDocument();
    expect(screen.getByText("1.79B")).toBeInTheDocument();
    expect(screen.getByText("$691.48")).toBeInTheDocument();
    expect(screen.getByText("$2304.93")).toBeInTheDocument();
    expect(screen.getByText("$1613.45")).toBeInTheDocument();

    const labels = [
      "剩余额度（估算）",
      "完整周期 Token 等效容量（估算）",
      "当前累计估算费用（USD）",
      "完整周期美元等效容量（估算）",
      "剩余额度 Token 等效容量（估算）",
      "剩余额度美元等效容量（估算）",
    ];
    for (const label of labels) {
      expect(screen.getByText(label)).toBeInTheDocument();
    }

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

  it("renders nothing when there is no priced local usage", () => {
    const { container } = render(
      <CodexCycleCapacityCard
        quota={quota}
        usage={{ ...usage, totalCost: "0" }}
        nowMs={queriedAt}
      />,
    );

    expect(container).toBeEmptyDOMElement();
  });
});
