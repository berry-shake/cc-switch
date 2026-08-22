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
            totalRequests: 1,
            totalCost: "1.25",
            totalInputTokens: 101,
            totalOutputTokens: 404,
            totalCacheCreationTokens: 202,
            totalCacheReadTokens: 303,
            successRate: 100,
            realTotalTokens: 1010,
            cacheHitRate: 0.5,
          },
        },
      ],
      isLoading: false,
    });
  });

  it("orders input, cache write, cache read, then output with matching values", () => {
    render(<UsageHero range={{ preset: "today" }} refreshIntervalMs={0} />);

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
    ).toEqual(["101", "202", "303", "404"]);
  });
});
