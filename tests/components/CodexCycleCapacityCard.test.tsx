import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { CodexCycleCapacityCard } from "@/components/usage/CodexCycleCapacityCard";
import type { SubscriptionQuota } from "@/types/subscription";
import type { UsageSummary } from "@/types/usage";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (_key: string, fallback?: string) => fallback ?? _key,
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
