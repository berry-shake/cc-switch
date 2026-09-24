import { describe, expect, it } from "vitest";

import {
  buildCodexAnalyticsUsageBasis,
  deriveCodexAnalyticsCycleCapacity,
  inspectCodexAnalyticsCycleData,
} from "@/lib/codexAnalyticsCapacity";
import { resolveCodexQuotaCycle } from "@/lib/codexCycleCapacity";
import type {
  CodexAnalyticsTokenCounts,
  CodexAnalyticsUsage,
  SubscriptionQuota,
} from "@/types/subscription";
import type { ModelPricing } from "@/types/usage";

const dayMs = 24 * 60 * 60 * 1000;
const queriedAt = Date.UTC(2026, 7, 22, 8);
const resetAt = Date.UTC(2026, 7, 25, 8);

const quota: SubscriptionQuota = {
  tool: "codex",
  credentialStatus: "valid",
  credentialMessage: null,
  success: true,
  tiers: [
    {
      name: "seven_day",
      windowSeconds: 7 * 24 * 60 * 60,
      utilization: 25,
      resetsAt: new Date(resetAt).toISOString(),
    },
  ],
  extraUsage: null,
  error: null,
  queriedAt,
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
  {
    modelId: "gpt-5.6-luna",
    displayName: "GPT-5.6 Luna",
    inputCostPerMillion: "0.2",
    outputCostPerMillion: "1.2",
    cacheReadCostPerMillion: "0.02",
    cacheCreationCostPerMillion: "0.25",
  },
];

function tokens(
  uncachedInputTokens: number,
  cachedInputTokens: number,
  cacheWriteInputTokens: number,
  outputTokens: number,
): CodexAnalyticsTokenCounts {
  return {
    uncachedInputTokens,
    cachedInputTokens,
    cacheWriteInputTokens,
    outputTokens,
    totalTokens:
      uncachedInputTokens +
      cachedInputTokens +
      cacheWriteInputTokens +
      outputTokens,
  };
}

describe("codexAnalyticsCapacity", () => {
  it.each([
    ["gpt-6-sol", "priority", "2", "10", "0.2", "2.5", 3.05],
    ["gpt-6-luna-xhigh", "fast", "0.1", "0.5", "0.01", "0.125", 0.1525],
    ["gpt-6-sol-high", "standard", "2", "10", "0.2", "2.5", 1.22],
  ])(
    "prices %s %s using subscription rates while keeping token counts",
    (model, speed, input, output, read, write, expected) => {
      const counts = tokens(100_000, 100_000, 100_000, 100_000);
      const analytics: CodexAnalyticsUsage = {
        accountMode: "workspace",
        queriedAt,
        days: [
          {
            date: "2026-08-22",
            missingTokenData: false,
            missingModelBreakdown: false,
            totals: counts,
            models: [
              {
                model: String(model),
                speed: String(speed),
                credits: 0,
                tokens: counts,
              },
            ],
          },
        ],
      };
      const pricing = [
        {
          modelId: String(model).replace(/-(high|xhigh)$/, ""),
          displayName: String(model),
          inputCostPerMillion: String(input),
          outputCostPerMillion: String(output),
          cacheReadCostPerMillion: String(read),
          cacheCreationCostPerMillion: String(write),
        },
      ];
      const result = deriveCodexAnalyticsCycleCapacity(
        quota,
        analytics,
        pricing,
        queriedAt,
      );
      expect(result?.usedUsd).toBeCloseTo(Number(expected), 8);
      expect(result?.usedTokens).toBe(400_000);
      expect(result?.hasUnknownPricing).toBe(false);
      expect(pricing[0].cacheCreationCostPerMillion).toBe(write);
    },
  );

  it("applies Astra Fast 2.5x without charging subscription cache writes", () => {
    const analytics: CodexAnalyticsUsage = {
      accountMode: "workspace",
      queriedAt,
      days: [
        {
          date: "2026-08-22",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(1_000_000, 1_000_000, 1_000_000, 1_000_000),
          models: [
            {
              model: "gpt-6-astra-high",
              speed: "fast",
              credits: 0,
              tokens: tokens(1_000_000, 1_000_000, 1_000_000, 1_000_000),
            },
          ],
        },
      ],
    };
    const pricing = [
      {
        modelId: "gpt-6-astra",
        displayName: "GPT-6 Astra",
        inputCostPerMillion: "10",
        outputCostPerMillion: "50",
        cacheReadCostPerMillion: "1",
        cacheCreationCostPerMillion: "12.5",
      },
    ];
    expect(
      deriveCodexAnalyticsCycleCapacity(quota, analytics, pricing, queriedAt)
        ?.usedUsd,
    ).toBe(152.5);
    analytics.days[0].models[0].speed = "standard";
    expect(
      deriveCodexAnalyticsCycleCapacity(quota, analytics, pricing, queriedAt)
        ?.usedUsd,
    ).toBe(61);
  });
  it("never silently falls back to token pricing for incomplete raw Credits", () => {
    const analytics: CodexAnalyticsUsage = {
      accountMode: "personal",
      queriedAt,
      days: [],
      personalCredits: {
        totalsStatus: "invalid",
        breakdownStatus: "available",
        days: [],
      },
    };
    expect(
      buildCodexAnalyticsUsageBasis(
        resolveCodexQuotaCycle(quota, queriedAt),
        analytics,
        modelPricing,
      ),
    ).toBeNull();
  });
  it("preserves workspace cache-write tokens without importing API write fees", () => {
    const analytics: CodexAnalyticsUsage = {
      accountMode: "workspace",
      queriedAt,
      days: [
        {
          date: "2026-08-22",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(1_000_000, 2_000_000, 100_000, 500_000),
          models: [
            {
              model: "gpt-5.6-sol",
              speed: "standard",
              credits: 0,
              tokens: tokens(1_000_000, 2_000_000, 100_000, 500_000),
            },
          ],
        },
      ],
    };

    const result = deriveCodexAnalyticsCycleCapacity(
      quota,
      analytics,
      modelPricing,
      queriedAt,
    );

    expect(result).not.toBeNull();
    expect(result?.usedTokens).toBe(3_600_000);
    expect(result?.usedUsd).toBeCloseTo(14.8, 6);
    expect(result?.totalTokens).toBe(14_400_000);
    expect(result?.totalUsd).toBeCloseTo(59.2, 6);
    expect(result?.hasUnknownPricing).toBe(false);
    expect(result?.hasEstimatedAllocation).toBe(false);
  });

  it("rate-adjusts personal multi-model credit shares without changing daily totals", () => {
    const analytics: CodexAnalyticsUsage = {
      accountMode: "personal",
      queriedAt,
      days: [
        {
          date: "2026-08-22",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(1_000_000, 1_000_000, 0, 1_000_000),
          models: [
            {
              model: "gpt-5.6-sol",
              speed: "fast",
              credits: 4,
              tokens: tokens(0, 0, 0, 0),
            },
            {
              model: "gpt-5.6-luna",
              speed: "standard",
              credits: 1,
              tokens: tokens(0, 0, 0, 0),
            },
          ],
        },
      ],
    };

    const basis = buildCodexAnalyticsUsageBasis(
      resolveCodexQuotaCycle(quota, queriedAt),
      analytics,
      modelPricing,
    );

    expect(basis).not.toBeNull();
    expect(basis?.usedTokens).toBe(3_000_000);
    expect(basis?.usedUsd).toBeGreaterThan(0);
    expect(basis?.usedUsd).toBeLessThan(61);
    expect(basis?.hasEstimatedAllocation).toBe(true);
    expect(basis?.hasUnknownPricing).toBe(false);
  });

  it("uses UTC day buckets and marks partially unknown model pricing", () => {
    const analytics: CodexAnalyticsUsage = {
      accountMode: "personal",
      queriedAt,
      days: [
        {
          date: "2026-08-17",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(9_000_000, 0, 0, 0),
          models: [
            {
              model: "gpt-5.6-sol",
              speed: "standard",
              credits: 1,
              tokens: tokens(0, 0, 0, 0),
            },
          ],
        },
        {
          date: "2026-08-18",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(1_000_000, 0, 0, 0),
          models: [
            {
              model: "gpt-5.6-sol",
              speed: "standard",
              credits: 1,
              tokens: tokens(0, 0, 0, 0),
            },
            {
              model: "private-model",
              speed: "standard",
              credits: 1,
              tokens: tokens(0, 0, 0, 0),
            },
          ],
        },
      ],
    };

    const basis = buildCodexAnalyticsUsageBasis(
      resolveCodexQuotaCycle(quota, queriedAt),
      analytics,
      modelPricing,
    );

    expect(basis?.includedDays).toBe(1);
    expect(basis?.usedTokens).toBe(1_000_000);
    expect(basis?.hasUnknownPricing).toBe(true);
    expect(basis?.hasEstimatedAllocation).toBe(true);
  });

  it("reports missing daily sources and a mid-day UTC cycle boundary", () => {
    const cycle = resolveCodexQuotaCycle(quota, queriedAt);
    const analytics: CodexAnalyticsUsage = {
      accountMode: "personal",
      queriedAt,
      days: [
        {
          date: "2026-08-18",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(1_000_000, 0, 0, 0),
          models: [],
        },
        {
          date: "2026-08-19",
          missingTokenData: true,
          missingModelBreakdown: false,
          totals: tokens(0, 0, 0, 0),
          models: [],
        },
        {
          date: "2026-08-20",
          missingTokenData: false,
          missingModelBreakdown: true,
          totals: tokens(1_000_000, 0, 0, 0),
          models: [],
        },
      ],
    };

    expect(inspectCodexAnalyticsCycleData(cycle, analytics)).toEqual({
      missingTokenDates: ["2026-08-19"],
      missingModelBreakdownDates: ["2026-08-20"],
      partialStartDate: "2026-08-18",
      // 起始日占已统计 Token 的一半，其中 08:00 之前的 1/3 可能属于上一周期。
      partialStartOverstatementRatio: expect.closeTo(1 / 6, 6),
    });
  });

  it("stays quiet when the cycle boundary can only distort a negligible share", () => {
    const cycle = resolveCodexQuotaCycle(quota, queriedAt);
    const analytics: CodexAnalyticsUsage = {
      accountMode: "personal",
      queriedAt,
      days: [
        {
          date: "2026-08-18",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(10_000, 0, 0, 0),
          models: [],
        },
        {
          date: "2026-08-21",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(4_000_000, 0, 0, 0),
          models: [],
        },
      ],
    };

    expect(inspectCodexAnalyticsCycleData(cycle, analytics)).toEqual({
      missingTokenDates: [],
      missingModelBreakdownDates: [],
      partialStartDate: null,
      partialStartOverstatementRatio: 0,
    });
  });

  it("returns null when the web data has no priceable usage", () => {
    const analytics: CodexAnalyticsUsage = {
      accountMode: "workspace",
      queriedAt,
      days: [
        {
          date: "2026-08-22",
          missingTokenData: false,
          missingModelBreakdown: false,
          totals: tokens(1_000_000, 0, 0, 0),
          models: [
            {
              model: "private-model",
              speed: "standard",
              credits: 0,
              tokens: tokens(1_000_000, 0, 0, 0),
            },
          ],
        },
      ],
    };

    expect(
      deriveCodexAnalyticsCycleCapacity(
        quota,
        analytics,
        modelPricing,
        queriedAt + dayMs,
      ),
    ).toBeNull();
  });
});
