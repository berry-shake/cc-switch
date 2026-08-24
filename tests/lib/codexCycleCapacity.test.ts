import { describe, expect, it } from "vitest";

import {
  deriveCodexCycleCapacity,
  estimateCodexCycleCapacity,
  resolveCodexQuotaCycle,
  resolveCodexQuotaCycleWindow,
} from "@/lib/codexCycleCapacity";
import type { SubscriptionQuota } from "@/types/subscription";
import type { UsageSummary } from "@/types/usage";

const QUERIED_AT = Date.UTC(2026, 7, 22, 8, 0, 0);
const RESET_AT = Date.UTC(2026, 7, 25, 8, 0, 0);
const SEVEN_DAYS = 7 * 24 * 60 * 60;

function makeQuota(
  overrides: Partial<SubscriptionQuota> = {},
): SubscriptionQuota {
  return {
    tool: "codex",
    credentialStatus: "valid",
    credentialMessage: null,
    success: true,
    tiers: [
      {
        name: "five_hour",
        windowSeconds: 18_000,
        utilization: 12,
        resetsAt: new Date(QUERIED_AT + 60 * 60 * 1000).toISOString(),
      },
      {
        name: "seven_day",
        windowSeconds: SEVEN_DAYS,
        utilization: 30,
        resetsAt: new Date(RESET_AT).toISOString(),
      },
    ],
    extraUsage: null,
    error: null,
    queriedAt: QUERIED_AT,
    ...overrides,
  };
}

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

describe("resolveCodexQuotaCycle", () => {
  it("selects the long Codex tier and returns the exact local query range", () => {
    const cycle = resolveCodexQuotaCycle(makeQuota(), QUERIED_AT);

    expect(cycle).toMatchObject({
      windowSeconds: SEVEN_DAYS,
      utilizationPercent: 30,
      usedRatio: 0.3,
      startMs: RESET_AT - SEVEN_DAYS * 1000,
      endMs: QUERIED_AT,
      resetAtMs: RESET_AT,
    });
    expect(cycle?.tier.name).toBe("seven_day");
  });

  it.each([
    ["missing quota", undefined],
    ["failed response", makeQuota({ success: false })],
    ["wrong tool", makeQuota({ tool: "claude" })],
    [
      "missing window length",
      makeQuota({
        tiers: [
          {
            name: "seven_day",
            utilization: 30,
            resetsAt: new Date(RESET_AT).toISOString(),
          },
        ],
      }),
    ],
    [
      "short window only",
      makeQuota({
        tiers: [
          {
            name: "five_hour",
            windowSeconds: 18_000,
            utilization: 12,
            resetsAt: new Date(QUERIED_AT + 60 * 60 * 1000).toISOString(),
          },
        ],
      }),
    ],
  ])("rejects %s", (_label, quota) => {
    expect(resolveCodexQuotaCycle(quota, QUERIED_AT)).toBeNull();
  });

  it("preserves a fresh zero-usage cycle without making it estimable", () => {
    const zeroQuota = makeQuota({
      tiers: [
        {
          name: "seven_day",
          windowSeconds: SEVEN_DAYS,
          utilization: 0,
          resetsAt: new Date(RESET_AT).toISOString(),
        },
      ],
    });
    const cycle = resolveCodexQuotaCycle(zeroQuota, QUERIED_AT);

    expect(cycle).toMatchObject({
      utilizationPercent: 0,
      usedRatio: 0,
      startMs: RESET_AT - SEVEN_DAYS * 1000,
      endMs: QUERIED_AT,
      resetAtMs: RESET_AT,
    });
    expect(estimateCodexCycleCapacity(cycle, usage)).toBeNull();
  });

  it("preserves a long cycle whose used percentage has not synced", () => {
    const quotaWithoutTier = makeQuota({ tiers: [] });
    const cycle = resolveCodexQuotaCycleWindow(
      quotaWithoutTier,
      [
        {
          usedPercent: null,
          windowSeconds: SEVEN_DAYS,
          resetsAt: new Date(RESET_AT).toISOString(),
        },
      ],
      QUERIED_AT,
    );

    expect(cycle).toEqual({
      windowSeconds: SEVEN_DAYS,
      utilizationPercent: null,
      startMs: RESET_AT - SEVEN_DAYS * 1000,
      endMs: QUERIED_AT,
      resetAtMs: RESET_AT,
    });
  });

  it("rejects a cached cycle after its reset time", () => {
    expect(resolveCodexQuotaCycle(makeQuota(), RESET_AT + 1)).toBeNull();
  });
});

describe("estimateCodexCycleCapacity", () => {
  it("projects full and remaining token/USD capacity from the used ratio", () => {
    const cycle = resolveCodexQuotaCycle(makeQuota(), QUERIED_AT);
    const result = estimateCodexCycleCapacity(cycle, usage);

    expect(result?.usedTokens).toBe(765_000_000);
    expect(result?.totalTokens).toBeCloseTo(2_550_000_000);
    expect(result?.remainingTokens).toBeCloseTo(1_785_000_000);
    expect(result?.usedUsd).toBeCloseTo(691.48);
    expect(result?.totalUsd).toBeCloseTo(2_304.933333);
    expect(result?.remainingUsd).toBeCloseTo(1_613.453333);
    expect(result?.remainingPercent).toBe(70);
  });

  it.each([
    ["zero tokens", { ...usage, realTotalTokens: 0 }],
    ["invalid cost", { ...usage, totalCost: "unknown" }],
    ["zero cost", { ...usage, totalCost: "0" }],
  ])("rejects local usage with %s", (_label, invalidUsage) => {
    expect(
      deriveCodexCycleCapacity(makeQuota(), invalidUsage, QUERIED_AT),
    ).toBeNull();
  });
});
