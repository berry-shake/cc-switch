import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CodexCycleCapacitySection } from "@/components/usage/CodexCycleCapacitySection";
import type { SubscriptionQuota } from "@/types/subscription";
import type { UsageSummary } from "@/types/usage";

const useQueryMock = vi.hoisted(() => vi.fn());
const getQuotaMock = vi.hoisted(() => vi.fn());
const getUsageSummaryMock = vi.hoisted(() => vi.fn());

vi.mock("@tanstack/react-query", () => ({
  useQuery: (options: unknown) => useQueryMock(options),
}));

vi.mock("@/lib/api/subscription", () => ({
  subscriptionApi: { getQuota: getQuotaMock },
}));

vi.mock("@/lib/api/usage", () => ({
  usageApi: { getUsageSummary: getUsageSummaryMock },
}));

vi.mock("@/components/usage/CodexCycleCapacityCard", () => ({
  CodexCycleCapacityCard: ({
    quota,
    usage,
  }: {
    quota: SubscriptionQuota;
    usage: UsageSummary;
  }) => (
    <div data-testid="capacity-entry">
      {quota.tool}:{usage.realTotalTokens}
    </div>
  ),
}));

const QUERIED_AT = Date.now();
const WINDOW_SECONDS = 7 * 24 * 60 * 60;
const RESET_AT = QUERIED_AT + 3 * 24 * 60 * 60 * 1000;

const quota: SubscriptionQuota = {
  tool: "codex",
  credentialStatus: "valid",
  credentialMessage: null,
  success: true,
  tiers: [
    {
      name: "seven_day",
      windowSeconds: WINDOW_SECONDS,
      utilization: 30,
      resetsAt: new Date(RESET_AT).toISOString(),
    },
  ],
  extraUsage: null,
  error: null,
  queriedAt: QUERIED_AT,
};

const usage: UsageSummary = {
  totalRequests: 1,
  totalCost: "10",
  totalInputTokens: 10,
  totalOutputTokens: 20,
  totalCacheCreationTokens: 30,
  totalCacheReadTokens: 40,
  successRate: 100,
  realTotalTokens: 100,
  cacheHitRate: 0.5,
};

function queryKind(options: { queryKey?: readonly unknown[] }) {
  return options.queryKey?.[0] === "subscription" ? "quota" : "usage";
}

describe("CodexCycleCapacitySection", () => {
  beforeEach(() => {
    useQueryMock.mockReset();
    getQuotaMock.mockReset();
    getUsageSummaryMock.mockReset();
  });

  it("shows the entry only after both current quota and cycle usage succeed", async () => {
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) =>
        queryKind(options) === "quota"
          ? { isSuccess: true, isError: false, data: quota }
          : { isSuccess: true, isError: false, data: usage },
    );

    render(<CodexCycleCapacitySection enabled refreshIntervalMs={0} />);

    expect(screen.getByTestId("capacity-entry")).toHaveTextContent("codex:100");
    const usageOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "usage");
    expect(usageOptions.enabled).toBe(true);

    getUsageSummaryMock.mockResolvedValue(usage);
    await usageOptions.queryFn();
    expect(getUsageSummaryMock).toHaveBeenCalledWith(
      Math.floor((RESET_AT - WINDOW_SECONDS * 1000) / 1000),
      Math.floor(QUERIED_AT / 1000),
      "codex",
      undefined,
      undefined,
    );
  });

  it("hides stale quota data when the latest API request rejected", () => {
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) =>
        queryKind(options) === "quota"
          ? { isSuccess: false, isError: true, data: quota }
          : { isSuccess: true, isError: false, data: usage },
    );

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.queryByTestId("capacity-entry")).not.toBeInTheDocument();
    const usageOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "usage");
    expect(usageOptions.enabled).toBe(false);
  });

  it("hides an HTTP/API failure returned as success=false", () => {
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) =>
        queryKind(options) === "quota"
          ? {
              isSuccess: true,
              isError: false,
              data: { ...quota, success: false, error: "HTTP 503" },
            }
          : { isSuccess: true, isError: false, data: usage },
    );

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.queryByTestId("capacity-entry")).not.toBeInTheDocument();
  });
});
