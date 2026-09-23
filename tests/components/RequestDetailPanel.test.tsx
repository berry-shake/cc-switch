import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { RequestDetailPanel } from "@/components/usage/RequestDetailPanel";

const useRequestDetailMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    i18n: { language: "en" },
  }),
}));

vi.mock("@/lib/query/usage", () => ({
  useRequestDetail: (...args: unknown[]) => useRequestDetailMock(...args),
}));

vi.mock("@/components/ui/dialog", () => ({
  Dialog: ({ children }: any) => <div>{children}</div>,
  DialogContent: ({ children }: any) => <div>{children}</div>,
  DialogHeader: ({ children }: any) => <div>{children}</div>,
  DialogTitle: ({ children }: any) => <h2>{children}</h2>,
}));

describe("RequestDetailPanel token metrics", () => {
  beforeEach(() => {
    useRequestDetailMock.mockReturnValue({
      data: {
        requestId: "request-1",
        providerId: "provider-1",
        providerName: "Provider",
        appType: "codex",
        model: "gpt-5.6-sol",
        costMultiplier: "1",
        inputTokens: 101,
        outputTokens: 404,
        cacheReadTokens: 303,
        cacheCreationTokens: 202,
        inputTokenSemantics: 2,
        inputCostUsd: "1.01",
        outputCostUsd: "4.04",
        cacheReadCostUsd: "3.03",
        cacheCreationCostUsd: "2.02",
        totalCostUsd: "10.10",
        isStreaming: true,
        latencyMs: 1000,
        statusCode: 200,
        createdAt: 1_700_000_000,
      },
      isLoading: false,
    });
  });

  it("orders token and cost rows as input, cache write, cache read, output", () => {
    render(<RequestDetailPanel requestId="request-1" onClose={() => {}} />);

    const tokenSection = screen.getByText("usage.tokenUsage").parentElement;
    const tokenLabels = Array.from(
      tokenSection?.querySelectorAll("dt") ?? [],
    ).map((label) => label.textContent);
    expect(tokenLabels.slice(0, 4)).toEqual([
      "usage.inputTokens",
      "usage.cacheCreationTokens",
      "usage.cacheReadTokens",
      "usage.outputTokens",
    ]);

    const costSection = screen.getByText("usage.costBreakdown").parentElement;
    const costLabels = Array.from(
      costSection?.querySelectorAll("dt") ?? [],
    ).map((label) => label.textContent?.replace("usage.baseCost", ""));
    expect(costLabels.slice(0, 4)).toEqual([
      "usage.inputCost()",
      "usage.cacheCreationCost()",
      "usage.cacheReadCost()",
      "usage.outputCost()",
    ]);
  });

  it("adds output speed without replacing the separate cache-write metric", () => {
    render(<RequestDetailPanel requestId="request-1" onClose={() => {}} />);

    const tokenSection = screen.getByText("usage.tokenUsage").parentElement;
    const metrics = new Map(
      Array.from(tokenSection?.querySelectorAll("dt") ?? []).map((label) => [
        label.textContent,
        label.nextElementSibling?.textContent,
      ]),
    );
    expect(metrics.get("usage.cacheCreationTokens")).toBe("202");
    expect(metrics.get("usage.cacheReadTokens")).toBe("303");
    expect(metrics.get("usage.outputTokens")).toBe("404(404 tps)");
  });
});
