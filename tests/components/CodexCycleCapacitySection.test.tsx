import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CodexCycleCapacitySection } from "@/components/usage/CodexCycleCapacitySection";
import { CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY } from "@/lib/codexCycleCapacityMode";
import type {
  CodexOfficialUsageSnapshot,
  CodexQuotaSnapshot,
  SubscriptionQuota,
} from "@/types/subscription";
import type { ModelPricing, UsageSummary } from "@/types/usage";

const useQueryMock = vi.hoisted(() => vi.fn());
const getQuotaSnapshotMock = vi.hoisted(() => vi.fn());
const getOfficialSnapshotMock = vi.hoisted(() => vi.fn());
const getUsageSummaryMock = vi.hoisted(() => vi.fn());
const getModelPricingMock = vi.hoisted(() => vi.fn());
const loadQuotaSamplesMock = vi.hoisted(() => vi.fn());
const localRefetchMock = vi.hoisted(() => vi.fn());
const officialRefetchMock = vi.hoisted(() => vi.fn());
const pricingRefetchMock = vi.hoisted(() => vi.fn());

vi.mock("@tanstack/react-query", () => ({
  useQuery: (options: unknown) => useQueryMock(options),
}));

vi.mock("@/lib/api/subscription", () => ({
  subscriptionApi: {
    getCodexQuotaSnapshot: getQuotaSnapshotMock,
    getCodexOfficialUsageSnapshot: getOfficialSnapshotMock,
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
    quotaWindows,
    usage,
    quotaSamples,
    analyticsUsage,
    analyticsUnavailable,
    accountEmail,
    lastRefreshedAt,
    calculationMode,
    onCalculationModeChange,
    onRefreshAnalytics,
  }: {
    quota: SubscriptionQuota;
    quotaWindows?: readonly unknown[];
    usage: UsageSummary | null;
    analyticsUsage?: { accountMode: string } | null;
    analyticsUnavailable?: boolean;
    accountEmail?: string | null;
    lastRefreshedAt?: number | null;
    quotaSamples?: readonly unknown[];
    calculationMode?: "local" | "analytics";
    onCalculationModeChange?: (mode: "local" | "analytics") => void;
    onRefreshAnalytics?: () => void | Promise<void>;
  }) => (
    <div data-testid="capacity-entry">
      {quota.tool}:{usage?.realTotalTokens ?? "no-local"}:
      {analyticsUsage?.accountMode ?? "no-analytics"}
      <span data-testid="sample-count">{quotaSamples?.length ?? 0}</span>
      <span data-testid="analytics-unavailable">
        {String(Boolean(analyticsUnavailable))}
      </span>
      <span data-testid="window-count">{quotaWindows?.length ?? 0}</span>
      <span data-testid="calculation-mode">{calculationMode}</span>
      <span data-testid="account-email">{accountEmail ?? "no-email"}</span>
      <span data-testid="last-refreshed-at">
        {lastRefreshedAt ?? "no-refresh"}
      </span>
      <button onClick={() => onCalculationModeChange?.("local")}>
        switch-local
      </button>
      <button onClick={() => onCalculationModeChange?.("analytics")}>
        switch-official
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

const localSnapshot: CodexQuotaSnapshot = {
  quota,
  quotaWindows: [
    {
      usedPercent: 30,
      windowSeconds: WINDOW_SECONDS,
      resetsAt: new Date(RESET_AT).toISOString(),
    },
  ],
  email: "local@example.com",
  credentialSource: "file",
  credentialScope: "scope-a",
};

const officialSnapshot: CodexOfficialUsageSnapshot = {
  ...localSnapshot,
  analytics: { accountMode: "personal", queriedAt: QUERIED_AT, days: [] },
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

type QueryOptions = {
  queryKey?: readonly unknown[];
  queryFn?: () => unknown;
  enabled?: boolean;
  refetchInterval?: number | false;
  staleTime?: number;
  refetchOnWindowFocus?: boolean;
};

function queryKind(options: QueryOptions) {
  if (options.queryKey?.[0] !== "subscription") {
    return options.queryKey?.[1] === "pricing" ? "pricing" : "usage";
  }
  return options.queryKey?.[1] === "codex-official-snapshot"
    ? "official"
    : "local";
}

function installQueries({
  local = localSnapshot,
  official = officialSnapshot,
  localSuccess = true,
  officialSuccess = true,
  officialStale = false,
  officialFetching = false,
  usageSuccess = true,
}: {
  local?: CodexQuotaSnapshot;
  official?: CodexOfficialUsageSnapshot;
  localSuccess?: boolean;
  officialSuccess?: boolean;
  officialStale?: boolean;
  officialFetching?: boolean;
  usageSuccess?: boolean;
} = {}) {
  useQueryMock.mockImplementation((options: QueryOptions) => {
    const kind = queryKind(options);
    if (kind === "local") {
      return {
        isSuccess: localSuccess,
        isError: !localSuccess,
        isFetching: false,
        data: local,
        refetch: localRefetchMock,
      };
    }
    if (kind === "official") {
      return {
        isSuccess: officialSuccess,
        isError: !officialSuccess,
        isFetching: officialFetching,
        isStale: officialStale,
        data: official,
        refetch: officialRefetchMock,
      };
    }
    if (kind === "pricing") {
      return {
        isSuccess: true,
        isError: false,
        isFetching: false,
        data: modelPricing,
        refetch: pricingRefetchMock,
      };
    }
    return {
      isSuccess: usageSuccess,
      isError: !usageSuccess,
      isFetching: false,
      data: usageSuccess ? usage : undefined,
      refetch: vi.fn(),
    };
  });
}

describe("CodexCycleCapacitySection", () => {
  beforeEach(() => {
    useQueryMock.mockReset();
    getQuotaSnapshotMock.mockReset();
    getOfficialSnapshotMock.mockReset();
    getUsageSummaryMock.mockReset();
    getModelPricingMock.mockReset();
    loadQuotaSamplesMock.mockReset();
    localRefetchMock.mockReset().mockResolvedValue(undefined);
    officialRefetchMock.mockReset().mockResolvedValue(undefined);
    pricingRefetchMock.mockReset().mockResolvedValue(undefined);
    loadQuotaSamplesMock.mockReturnValue([{ capturedAtMs: QUERIED_AT }]);
    window.localStorage.removeItem(CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY);
  });

  it("uses scoped local quota samples and keeps official data account-consistent", async () => {
    installQueries();
    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("capacity-entry")).toHaveTextContent(
      "codex:100:personal",
    );
    expect(loadQuotaSamplesMock).toHaveBeenCalledWith("scope-a");
    expect(screen.getByTestId("account-email")).toHaveTextContent(
      "local@example.com",
    );
    expect(screen.getByTestId("last-refreshed-at")).toHaveTextContent(
      String(QUERIED_AT),
    );

    const options = useQueryMock.mock.calls.map(
      ([value]) => value as QueryOptions,
    );
    const localOptions = options.find((value) => queryKind(value) === "local")!;
    const officialOptions = options.find(
      (value) => queryKind(value) === "official",
    )!;
    // 两种模式打同一个额度端点，本地模式不得比官方模式更频繁。
    expect(localOptions.refetchInterval).toBe(60 * 60 * 1000);
    expect(localOptions.staleTime).toBe(60 * 60 * 1000);
    expect(localOptions.refetchOnWindowFocus).toBe(true);
    expect(officialOptions.refetchInterval).toBe(false);

    getQuotaSnapshotMock.mockResolvedValue(localSnapshot);
    getOfficialSnapshotMock.mockResolvedValue(officialSnapshot);
    await localOptions.queryFn?.();
    await officialOptions.queryFn?.();
    expect(getQuotaSnapshotMock).toHaveBeenCalledTimes(1);
    expect(getOfficialSnapshotMock).toHaveBeenCalledTimes(1);
  });

  it("forwards partial analytics failure only for the active official snapshot", () => {
    installQueries({
      official: {
        ...officialSnapshot,
        analytics: null,
        analyticsUnavailable: true,
      },
    });
    render(<CodexCycleCapacitySection enabled />);
    expect(screen.getByTestId("analytics-unavailable")).toHaveTextContent(
      "false",
    );
    fireEvent.click(screen.getByText("switch-official"));
    expect(screen.getByTestId("analytics-unavailable")).toHaveTextContent(
      "true",
    );
    expect(screen.getByTestId("capacity-entry")).toBeInTheDocument();
    expect(screen.getByTestId("account-email")).toHaveTextContent(
      officialSnapshot.email!,
    );
  });

  it("polls and manually refreshes one atomic official snapshot", async () => {
    window.localStorage.setItem(
      CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
      "analytics",
    );
    installQueries();
    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("calculation-mode")).toHaveTextContent(
      "analytics",
    );
    expect(screen.getByTestId("account-email")).toHaveTextContent(
      "local@example.com",
    );
    const options = useQueryMock.mock.calls.map(
      ([value]) => value as QueryOptions,
    );
    const localOptions = options.find((value) => queryKind(value) === "local")!;
    const officialOptions = options.find(
      (value) => queryKind(value) === "official",
    )!;
    expect(localOptions.enabled).toBe(false);
    expect(officialOptions.refetchInterval).toBe(60 * 60 * 1000);
    expect(officialOptions.staleTime).toBe(60 * 60 * 1000);
    expect(officialOptions.refetchOnWindowFocus).toBe(false);

    fireEvent.click(screen.getByText("refresh-official"));
    await waitFor(() => {
      expect(officialRefetchMock).toHaveBeenCalledTimes(1);
      expect(pricingRefetchMock).toHaveBeenCalledTimes(1);
    });
    expect(localRefetchMock).not.toHaveBeenCalled();
  });

  it("reuses a fresh official snapshot when switching from local mode", () => {
    installQueries();
    render(<CodexCycleCapacitySection enabled />);

    fireEvent.click(screen.getByText("switch-official"));

    expect(screen.getByTestId("calculation-mode")).toHaveTextContent(
      "analytics",
    );
    expect(officialRefetchMock).not.toHaveBeenCalled();
    expect(pricingRefetchMock).not.toHaveBeenCalled();
  });

  it("refreshes a stale official snapshot when switching from local mode", async () => {
    installQueries({ officialStale: true });
    render(<CodexCycleCapacitySection enabled />);

    fireEvent.click(screen.getByText("switch-official"));

    await waitFor(() => {
      expect(officialRefetchMock).toHaveBeenCalledTimes(1);
      expect(pricingRefetchMock).toHaveBeenCalledTimes(1);
    });
  });

  it("does not duplicate a stale official refresh already in flight", () => {
    installQueries({ officialStale: true, officialFetching: true });
    render(<CodexCycleCapacitySection enabled />);

    fireEvent.click(screen.getByText("switch-official"));

    expect(screen.getByTestId("calculation-mode")).toHaveTextContent(
      "analytics",
    );
    expect(officialRefetchMock).not.toHaveBeenCalled();
    expect(pricingRefetchMock).not.toHaveBeenCalled();
  });

  it("does not combine analytics from another credential scope", () => {
    installQueries({
      official: { ...officialSnapshot, credentialScope: "scope-b" },
    });
    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("capacity-entry")).toHaveTextContent(
      "codex:100:no-analytics",
    );
  });

  it("renders a successful fresh 0% cycle instead of hiding the section", () => {
    window.localStorage.setItem(
      CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
      "analytics",
    );
    const zeroQuota: SubscriptionQuota = {
      ...quota,
      tiers: [{ ...quota.tiers[0], utilization: 0 }],
    };
    installQueries({
      official: {
        ...officialSnapshot,
        quota: zeroQuota,
        quotaWindows: [
          {
            usedPercent: 0,
            windowSeconds: WINDOW_SECONDS,
            resetsAt: new Date(RESET_AT).toISOString(),
          },
        ],
        analytics: null,
      },
    });

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("capacity-entry")).toBeInTheDocument();
    expect(screen.getByTestId("window-count")).toHaveTextContent("1");
    expect(screen.getByTestId("calculation-mode")).toHaveTextContent(
      "analytics",
    );
  });

  it("renders a successful snapshot with a cycle whose percentage is missing", () => {
    window.localStorage.setItem(
      CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
      "analytics",
    );
    installQueries({
      official: {
        ...officialSnapshot,
        quota: { ...quota, tiers: [] },
        quotaWindows: [
          {
            usedPercent: null,
            windowSeconds: WINDOW_SECONDS,
            resetsAt: new Date(RESET_AT).toISOString(),
          },
        ],
        analytics: null,
      },
    });

    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("capacity-entry")).toBeInTheDocument();
    expect(screen.getByTestId("window-count")).toHaveTextContent("1");
    expect(loadQuotaSamplesMock).not.toHaveBeenCalled();

    const options = useQueryMock.mock.calls.map(
      ([value]) => value as QueryOptions,
    );
    expect(options.find((value) => queryKind(value) === "usage")?.enabled).toBe(
      false,
    );
    expect(
      options.find((value) => queryKind(value) === "pricing")?.enabled,
    ).toBe(false);
  });

  it("hides stale official data when the atomic refresh rejects", () => {
    window.localStorage.setItem(
      CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
      "analytics",
    );
    installQueries({ officialSuccess: false });
    render(<CodexCycleCapacitySection enabled />);

    expect(screen.queryByTestId("capacity-entry")).not.toBeInTheDocument();
    expect(loadQuotaSamplesMock).not.toHaveBeenCalled();
  });
});
