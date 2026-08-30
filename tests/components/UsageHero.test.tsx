import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { UsageHero } from "@/components/usage/UsageHero";

const useUsageSummaryByAppMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    i18n: { resolvedLanguage: "en", language: "en" },
  }),
}));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...props }: any) => <div {...props}>{children}</div>,
  },
}));

vi.mock("@/lib/query/usage", () => ({
  useUsageSummaryByApp: (...args: unknown[]) =>
    useUsageSummaryByAppMock(...args),
}));

describe("UsageHero token metrics", () => {
  beforeEach(() => {
    useUsageSummaryByAppMock.mockReturnValue({
      data: [
        {
          appType: "claude",
          summary: {
            totalRequests: 2_947,
            totalCost: "225.9919",
            totalInputTokens: 8_622_000,
            totalOutputTokens: 1_040_000,
            totalCacheCreationTokens: 0,
            totalCacheReadTokens: 370_000_000,
            successRate: 100,
            realTotalTokens: 379_662_000,
            cacheHitRate: 0.97,
          },
        },
      ],
      isLoading: false,
    });
  });

  it("uses K, M, and B units for every count while preserving metric order", () => {
    render(<UsageHero range={{ preset: "today" }} refreshIntervalMs={0} />);

    expect(screen.getByText("379.66M")).toBeInTheDocument();
    expect(screen.getByText("2.95K")).toBeInTheDocument();

    const labels = screen.getAllByText(
      /^usage\.(inputTokens|cacheCreationTokens|cacheReadTokens|outputTokens)$/,
    );
    expect(labels.map((label) => label.textContent)).toEqual([
      "usage.inputTokens",
      "usage.cacheCreationTokens",
      "usage.cacheReadTokens",
      "usage.outputTokens",
    ]);
    expect(
      labels.map((label) => label.parentElement?.nextSibling?.textContent),
    ).toEqual(["8.62M", "0", "370M", "1.04M"]);
  });
});
