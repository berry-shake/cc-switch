import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  CodexOfficialUsageSnapshot,
  CodexQuotaSnapshot,
  SubscriptionQuota,
} from "@/types/subscription";

const invokeMock = vi.hoisted(() => vi.fn());
const recordQuotaSampleMock = vi.hoisted(() => vi.fn());
const sampleQuotaCycleMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

vi.mock("@/lib/codexQuotaSamples", () => ({
  recordCodexQuotaSample: recordQuotaSampleMock,
  sampleCodexQuotaCycle: sampleQuotaCycleMock,
}));

import { subscriptionApi } from "@/lib/api/subscription";

const DAY_MS = 24 * 60 * 60 * 1000;
const QUERIED_AT = Date.UTC(2026, 7, 22, 8);
const RESET_AT = QUERIED_AT + 3 * DAY_MS;

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
      resetsAt: new Date(RESET_AT).toISOString(),
    },
  ],
  extraUsage: null,
  error: null,
  queriedAt: QUERIED_AT,
};

const quotaSnapshot: CodexQuotaSnapshot = {
  quota,
  credentialSource: "file",
  credentialScope: "scope-a",
};

describe("subscriptionApi Codex quota sampling", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(QUERIED_AT);
    invokeMock.mockReset();
    recordQuotaSampleMock.mockReset();
    sampleQuotaCycleMock.mockReset();
    sampleQuotaCycleMock.mockReturnValue({ capturedAtMs: QUERIED_AT });
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps the generic quota endpoint free of unscoped sample writes", async () => {
    invokeMock.mockResolvedValue(quota);

    await expect(subscriptionApi.getQuota("codex")).resolves.toBe(quota);

    expect(invokeMock).toHaveBeenCalledWith("get_subscription_quota", {
      tool: "codex",
    });
    expect(sampleQuotaCycleMock).not.toHaveBeenCalled();
    expect(recordQuotaSampleMock).not.toHaveBeenCalled();
  });

  it("records a long-cycle response under its anonymous credential scope", async () => {
    invokeMock.mockResolvedValue(quotaSnapshot);

    await expect(subscriptionApi.getCodexQuotaSnapshot()).resolves.toBe(
      quotaSnapshot,
    );

    expect(invokeMock).toHaveBeenCalledWith("get_codex_quota_snapshot");
    expect(sampleQuotaCycleMock).toHaveBeenCalledWith(
      expect.objectContaining({
        startMs: RESET_AT - 7 * DAY_MS,
        endMs: QUERIED_AT,
        resetAtMs: RESET_AT,
        utilizationPercent: 30,
      }),
      "scope-a",
    );
    expect(recordQuotaSampleMock).toHaveBeenCalledWith({
      capturedAtMs: QUERIED_AT,
    });
  });

  it("does not record a failed scoped response", async () => {
    invokeMock.mockResolvedValue({
      ...quotaSnapshot,
      quota: { ...quota, success: false },
    });

    await subscriptionApi.getCodexQuotaSnapshot();

    expect(sampleQuotaCycleMock).not.toHaveBeenCalled();
    expect(recordQuotaSampleMock).not.toHaveBeenCalled();
  });

  it("preserves a rejected quota request without writing a sample", async () => {
    invokeMock.mockRejectedValue(new Error("offline"));

    await expect(subscriptionApi.getCodexQuotaSnapshot()).rejects.toThrow(
      "offline",
    );
    expect(recordQuotaSampleMock).not.toHaveBeenCalled();
  });

  it("records the quota embedded in an atomic official snapshot", async () => {
    const official: CodexOfficialUsageSnapshot = {
      ...quotaSnapshot,
      analytics: { accountMode: "personal", days: [], queriedAt: QUERIED_AT },
      queriedAt: QUERIED_AT,
    };
    invokeMock.mockResolvedValue(official);

    await expect(subscriptionApi.getCodexOfficialUsageSnapshot()).resolves.toBe(
      official,
    );

    expect(invokeMock).toHaveBeenCalledWith(
      "get_codex_official_usage_snapshot",
    );
    expect(sampleQuotaCycleMock).toHaveBeenCalledWith(
      expect.any(Object),
      "scope-a",
    );
    expect(recordQuotaSampleMock).toHaveBeenCalledTimes(1);
  });

  it("forwards the UTC daily analytics range without exposing credentials", async () => {
    const analytics = {
      accountMode: "personal",
      days: [],
      queriedAt: QUERIED_AT,
    };
    invokeMock.mockResolvedValue(analytics);

    await expect(
      subscriptionApi.getCodexUsageAnalytics("2026-08-17", "2026-08-23"),
    ).resolves.toBe(analytics);

    expect(invokeMock).toHaveBeenCalledWith("get_codex_usage_analytics", {
      startDate: "2026-08-17",
      endDate: "2026-08-23",
    });
  });
});
