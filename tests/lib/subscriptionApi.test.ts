import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { SubscriptionQuota } from "@/types/subscription";

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

  it("records every successful Codex CLI long-cycle response", async () => {
    invokeMock.mockResolvedValue(quota);

    await expect(subscriptionApi.getQuota("codex")).resolves.toBe(quota);

    expect(invokeMock).toHaveBeenCalledWith("get_subscription_quota", {
      tool: "codex",
    });
    expect(sampleQuotaCycleMock).toHaveBeenCalledWith(
      expect.objectContaining({
        startMs: RESET_AT - 7 * DAY_MS,
        endMs: QUERIED_AT,
        resetAtMs: RESET_AT,
        utilizationPercent: 30,
      }),
    );
    expect(recordQuotaSampleMock).toHaveBeenCalledWith({
      capturedAtMs: QUERIED_AT,
    });
  });

  it.each([
    ["failed Codex response", "codex", { ...quota, success: false }],
    ["non-Codex response", "claude", { ...quota, tool: "claude" }],
  ])("does not record a %s", async (_label, tool, response) => {
    invokeMock.mockResolvedValue(response);

    await subscriptionApi.getQuota(tool);

    expect(sampleQuotaCycleMock).not.toHaveBeenCalled();
    expect(recordQuotaSampleMock).not.toHaveBeenCalled();
  });

  it("preserves a rejected quota request without writing a sample", async () => {
    invokeMock.mockRejectedValue(new Error("offline"));

    await expect(subscriptionApi.getQuota("codex")).rejects.toThrow("offline");
    expect(recordQuotaSampleMock).not.toHaveBeenCalled();
  });
});
