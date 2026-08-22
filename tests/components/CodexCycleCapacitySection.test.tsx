import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CodexCycleCapacitySection } from "@/components/usage/CodexCycleCapacitySection";
import { CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY } from "@/lib/codexCycleCapacityMode";
import type {
  CodexAnalyticsUsage,
  SubscriptionQuota,
} from "@/types/subscription";
import type { ModelPricing, UsageSummary } from "@/types/usage";

const useQueryMock = vi.hoisted(() => vi.fn());
const getQuotaMock = vi.hoisted(() => vi.fn());
const getCodexUsageAnalyticsMock = vi.hoisted(() => vi.fn());
const getUsageSummaryMock = vi.hoisted(() => vi.fn());
const getModelPricingMock = vi.hoisted(() => vi.fn());
const loadQuotaSamplesMock = vi.hoisted(() => vi.fn());
const quotaRefetchMock = vi.hoisted(() => vi.fn());
const analyticsRefetchMock = vi.hoisted(() => vi.fn());
const pricingRefetchMock = vi.hoisted(() => vi.fn());

vi.mock("@tanstack/react-query", () => ({
  useQuery: (options: unknown) => useQueryMock(options),
}));

vi.mock("@/lib/api/subscription", () => ({
  subscriptionApi: {
    getQuota: getQuotaMock,
    getCodexUsageAnalytics: getCodexUsageAnalyticsMock,
  },
}));

vi.mock("@/lib/api/usage", () => ({
  usageApi: {
    getUsageSummary: getUsageSummaryMock,
    getModelPricing: getModelPricingMock,
  },
}));

vi.mock("@/lib/codexQuotaSamples", () => ({
  loadCodexQuotaSamples: loadQuotaSamplesMock,
}));

vi.mock("@/components/usage/CodexCycleCapacityCard", () => ({
  CodexCycleCapacityCard: ({
    quota,
    usage,
    quotaSamples,
    analyticsUsage,
    calculationMode,
    onCalculationModeChange,
    onRefreshAnalytics,
  }: {
    quota: SubscriptionQuota;
    usage: UsageSummary | null;
    analyticsUsage?: CodexAnalyticsUsage | null;
    quotaSamples?: readonly unknown[];
    calculationMode?: "local" | "analytics";
    onCalculationModeChange?: (mode: "local" | "analytics") => void;
    onRefreshAnalytics?: () => void | Promise<void>;
  }) => (
    <div data-testid="capacity-entry">
      {quota.tool}:{usage?.realTotalTokens ?? "no-local"}:
      {analyticsUsage?.accountMode ?? "no-analytics"}
      <span data-testid="sample-count">{quotaSamples?.length ?? 0}</span>
      <span data-testid="calculation-mode">{calculationMode}</span>
      <button onClick={() => onCalculationModeChange?.("analytics")}>
        select-official
      </button>
      <button onClick={() => void onRefreshAnalytics?.()}>
        refresh-official
      </button>
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

const analyticsUsage: CodexAnalyticsUsage = {
  accountMode: "personal",
  queriedAt: QUERIED_AT,
  days: [],
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

function queryKind(options: { queryKey?: readonly unknown[] }) {
  if (options.queryKey?.[0] !== "subscription") {
    return options.queryKey?.[1] === "pricing" ? "pricing" : "usage";
  }
  return options.queryKey?.[1] === "codex-analytics" ? "analytics" : "quota";
}

describe("CodexCycleCapacitySection", () => {
  beforeEach(() => {
    useQueryMock.mockReset();
    getQuotaMock.mockReset();
    getCodexUsageAnalyticsMock.mockReset();
    getUsageSummaryMock.mockReset();
    getModelPricingMock.mockReset();
    loadQuotaSamplesMock.mockReset();
    quotaRefetchMock.mockReset();
    analyticsRefetchMock.mockReset();
    pricingRefetchMock.mockReset();
    quotaRefetchMock.mockResolvedValue(undefined);
    analyticsRefetchMock.mockResolvedValue(undefined);
    pricingRefetchMock.mockResolvedValue(undefined);
    loadQuotaSamplesMock.mockReturnValue([{ capturedAtMs: QUERIED_AT }]);
    window.localStorage.removeItem(CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY);
  });

  it("shows the entry only after both current quota and cycle usage succeed", async () => {
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) =>
        queryKind(options) === "quota"
          ? { isSuccess: true, isError: false, data: quota }
          : queryKind(options) === "analytics"
            ? { isSuccess: true, isError: false, data: analyticsUsage }
            : queryKind(options) === "pricing"
              ? { isSuccess: true, isError: false, data: modelPricing }
              : { isSuccess: true, isError: false, data: usage },
    );

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("capacity-entry")).toHaveTextContent(
      "codex:100:personal",
    );
    expect(screen.getByTestId("sample-count")).toHaveTextContent("1");
    expect(loadQuotaSamplesMock).toHaveBeenCalledTimes(1);
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

    const analyticsOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "analytics");
    expect(analyticsOptions.enabled).toBe(true);
    expect(analyticsOptions.refetchInterval).toBe(false);
    expect(analyticsOptions.refetchOnWindowFocus).toBe(false);
    const quotaOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "quota");
    expect(quotaOptions.refetchInterval).toBe(5 * 60 * 1000);
    expect(quotaOptions.refetchOnWindowFocus).toBe(true);
    getCodexUsageAnalyticsMock.mockResolvedValue(analyticsUsage);
    await analyticsOptions.queryFn();
    expect(getCodexUsageAnalyticsMock).toHaveBeenCalledWith(
      new Date(RESET_AT - WINDOW_SECONDS * 1000 - 24 * 60 * 60 * 1000)
        .toISOString()
        .slice(0, 10),
      new Date(QUERIED_AT + 24 * 60 * 60 * 1000).toISOString().slice(0, 10),
    );
  });

  it("polls official usage hourly only while official API mode is selected", async () => {
    window.localStorage.setItem(
      CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
      "analytics",
    );
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) => {
        const kind = queryKind(options);
        return kind === "quota"
          ? {
              isSuccess: true,
              isError: false,
              isFetching: false,
              data: quota,
              refetch: quotaRefetchMock,
            }
          : kind === "analytics"
            ? {
                isSuccess: true,
                isError: false,
                isFetching: false,
                data: analyticsUsage,
                refetch: analyticsRefetchMock,
              }
            : kind === "pricing"
              ? {
                  isSuccess: true,
                  isError: false,
                  isFetching: false,
                  data: modelPricing,
                  refetch: pricingRefetchMock,
                }
              : {
                  isSuccess: true,
                  isError: false,
                  isFetching: false,
                  data: usage,
                  refetch: vi.fn(),
                };
      },
    );

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("calculation-mode")).toHaveTextContent(
      "analytics",
    );
    const analyticsOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "analytics");
    expect(analyticsOptions.refetchInterval).toBe(60 * 60 * 1000);
    expect(analyticsOptions.staleTime).toBe(60 * 60 * 1000);
    expect(analyticsOptions.refetchIntervalInBackground).toBe(false);
    expect(analyticsOptions.refetchOnWindowFocus).toBe(false);
    const quotaOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "quota");
    expect(quotaOptions.refetchInterval).toBe(60 * 60 * 1000);
    expect(quotaOptions.staleTime).toBe(60 * 60 * 1000);
    expect(quotaOptions.refetchOnWindowFocus).toBe(false);

    fireEvent.click(screen.getByText("refresh-official"));
    await waitFor(() => {
      expect(quotaRefetchMock).toHaveBeenCalledTimes(1);
      expect(analyticsRefetchMock).toHaveBeenCalledTimes(1);
      expect(pricingRefetchMock).toHaveBeenCalledTimes(1);
    });
  });

  it("keeps the same card available when only web analytics succeeds", () => {
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) =>
        queryKind(options) === "quota"
          ? { isSuccess: true, isError: false, data: quota }
          : queryKind(options) === "analytics"
            ? { isSuccess: true, isError: false, data: analyticsUsage }
            : queryKind(options) === "pricing"
              ? { isSuccess: true, isError: false, data: modelPricing }
              : { isSuccess: false, isError: true, data: undefined },
    );

    render(<CodexCycleCapacitySection enabled refreshIntervalMs={0} />);

    expect(screen.getByTestId("capacity-entry")).toHaveTextContent(
      "codex:no-local:personal",
    );
  });

  it("hides stale quota data when the latest API request rejected", () => {
    useQueryMock.mockImplementation(
      (options: { queryKey?: readonly unknown[] }) =>
        queryKind(options) === "quota"
          ? { isSuccess: false, isError: true, data: quota }
          : queryKind(options) === "analytics"
            ? { isSuccess: true, isError: false, data: analyticsUsage }
            : queryKind(options) === "pricing"
              ? { isSuccess: true, isError: false, data: modelPricing }
              : { isSuccess: true, isError: false, data: usage },
    );

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.queryByTestId("capacity-entry")).not.toBeInTheDocument();
    expect(loadQuotaSamplesMock).not.toHaveBeenCalled();
    const usageOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "usage");
    expect(usageOptions.enabled).toBe(false);
    const analyticsOptions = useQueryMock.mock.calls
      .map(([options]) => options)
      .find((options) => queryKind(options) === "analytics");
    expect(analyticsOptions.enabled).toBe(false);
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
          : queryKind(options) === "analytics"
            ? { isSuccess: true, isError: false, data: analyticsUsage }
            : queryKind(options) === "pricing"
              ? { isSuccess: true, isError: false, data: modelPricing }
              : { isSuccess: true, isError: false, data: usage },
    );

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.queryByTestId("capacity-entry")).not.toBeInTheDocument();
    expect(loadQuotaSamplesMock).not.toHaveBeenCalled();
  });
});
