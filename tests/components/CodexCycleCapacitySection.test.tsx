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
    usage,
    quotaSamples,
    analyticsUsage,
    calculationMode,
    onRefreshAnalytics,
  }: {
    quota: SubscriptionQuota;
    usage: UsageSummary | null;
    analyticsUsage?: { accountMode: string } | null;
    quotaSamples?: readonly unknown[];
    calculationMode?: "local" | "analytics";
    onRefreshAnalytics?: () => void | Promise<void>;
  }) => (
    <div data-testid="capacity-entry">
      {quota.tool}:{usage?.realTotalTokens ?? "no-local"}:
      {analyticsUsage?.accountMode ?? "no-analytics"}
      <span data-testid="sample-count">{quotaSamples?.length ?? 0}</span>
      <span data-testid="calculation-mode">{calculationMode}</span>
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
  usageSuccess = true,
}: {
  local?: CodexQuotaSnapshot;
  official?: CodexOfficialUsageSnapshot;
  localSuccess?: boolean;
  officialSuccess?: boolean;
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
        isFetching: false,
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

    const options = useQueryMock.mock.calls.map(
      ([value]) => value as QueryOptions,
    );
    const localOptions = options.find((value) => queryKind(value) === "local")!;
    const officialOptions = options.find(
      (value) => queryKind(value) === "official",
    )!;
    expect(localOptions.refetchInterval).toBe(5 * 60 * 1000);
    expect(localOptions.refetchOnWindowFocus).toBe(true);
    expect(officialOptions.refetchInterval).toBe(false);

    getQuotaSnapshotMock.mockResolvedValue(localSnapshot);
    getOfficialSnapshotMock.mockResolvedValue(officialSnapshot);
    await localOptions.queryFn?.();
    await officialOptions.queryFn?.();
    expect(getQuotaSnapshotMock).toHaveBeenCalledTimes(1);
    expect(getOfficialSnapshotMock).toHaveBeenCalledTimes(1);
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

  it("does not combine analytics from another credential scope", () => {
    installQueries({
      official: { ...officialSnapshot, credentialScope: "scope-b" },
    });
    render(<CodexCycleCapacitySection enabled />);

    expect(screen.getByTestId("capacity-entry")).toHaveTextContent(
      "codex:100:no-analytics",
    );
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
