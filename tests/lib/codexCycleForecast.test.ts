import { describe, expect, it } from "vitest";

import {
  forecastCodexCycle,
  type CodexCycleForecastInput,
} from "@/lib/codexCycleForecast";

const HOUR_MS = 60 * 60 * 1000;
const DAY_MS = 24 * HOUR_MS;
const START_MS = Date.UTC(2026, 7, 18, 8, 0, 0);
const RESET_MS = START_MS + 7 * DAY_MS;
const QUERIED_MS = START_MS + 2 * DAY_MS;

function makeInput(
  overrides: Partial<CodexCycleForecastInput> = {},
): CodexCycleForecastInput {
  return {
    cycleStartMs: START_MS,
    resetAtMs: RESET_MS,
    queriedAtMs: QUERIED_MS,
    utilizationPercent: 30,
    samples: [],
    ...overrides,
  };
}

describe("forecastCodexCycle cumulative forecast", () => {
  it("uses current utilization divided by elapsed cycle time", () => {
    const result = forecastCodexCycle(makeInput());

    expect(result?.cycleTimeProgressRatio).toBeCloseTo(2 / 7);
    expect(result?.cycleTimeProgressPercent).toBeCloseTo((2 / 7) * 100);
    expect(result?.baselineUtilizationPercent).toBeCloseTo((2 / 7) * 100);
    expect(result?.elapsedMs).toBe(2 * DAY_MS);
    expect(result?.remainingMs).toBe(5 * DAY_MS);
    expect(result?.sustainableRatePercentPerDay).toBeCloseTo(100 / 7);
    expect(result?.cumulativeRatePercentPerDay).toBeCloseTo(15);
    expect(result?.cumulativeProjectedUtilizationAtReset).toBe(105);
    expect(result?.cumulativeExhaustion).toMatchObject({
      kind: "at",
      withinCycle: true,
    });
    expect(result?.cumulativeExhaustion.atMs).toBeCloseTo(
      START_MS + (100 / 15) * DAY_MS,
      -2,
    );
    expect(result?.paceRatio).toBeCloseTo(1.05);
    expect(result?.statusLevel).toBe("on_pace");
  });

  it("ignores history when calculating the cumulative rate", () => {
    const result = forecastCodexCycle(
      makeInput({
        samples: [
          { timestampMs: START_MS + DAY_MS, utilizationPercent: 29 },
          { timestampMs: QUERIED_MS - 2 * HOUR_MS, utilizationPercent: 29.5 },
        ],
      }),
    );

    expect(result?.cumulativeRatePercentPerDay).toBeCloseTo(15);
  });

  it("returns an infinite depletion result for a zero cumulative rate", () => {
    const result = forecastCodexCycle(makeInput({ utilizationPercent: 0 }));

    expect(result?.cumulativeRatePercentPerDay).toBe(0);
    expect(result?.cumulativeExhaustion).toEqual({
      kind: "never",
      atMs: null,
      withinCycle: false,
    });
    expect(result?.cumulativeProjectedUtilizationAtReset).toBe(0);
    expect(result?.statusLevel).toBe("below_pace");
  });

  it("reproduces the reference forecast from its exact elapsed and recent spans", () => {
    const elapsedMs = DAY_MS + 23 * HOUR_MS + 16 * 60 * 1000;
    const recentSpanMs = 14 * HOUR_MS + 48 * 60 * 1000;
    const queriedAtMs = START_MS + elapsedMs;
    const result = forecastCodexCycle(
      makeInput({
        queriedAtMs,
        utilizationPercent: 30,
        samples: [
          {
            timestampMs: queriedAtMs - recentSpanMs,
            utilizationPercent: 15,
          },
        ],
      }),
    );

    expect(result?.elapsedMs).toBe(elapsedMs);
    expect(result?.remainingMs).toBe(RESET_MS - queriedAtMs);
    expect(result?.cycleTimeProgressPercent).toBeCloseTo(28.1349, 3);
    expect(result?.baselineUtilizationPercent).toBeCloseTo(28.1349, 3);
    expect(result?.cumulativeRatePercentPerDay).toBeCloseTo(15.2327, 3);
    expect(result?.sustainableRatePercentPerDay).toBeCloseTo(14.2857, 3);
    expect(result?.recentWindowSpanMs).toBe(recentSpanMs);
    expect(result?.recentRatePercentPerDay).toBeCloseTo(24.3243, 3);
    expect(result?.recentProjectedUtilizationAtReset).toBeCloseTo(152.3649, 3);
    expect(result?.statusLevel).toBe("on_pace");
  });

  it("treats exhaustion exactly at reset as on-time rather than early", () => {
    const sustainableRate = 100 / 7;
    const recentSpanMs = 2 * HOUR_MS;
    const currentUtilization = sustainableRate * 2;
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: currentUtilization,
        samples: [
          {
            timestampMs: QUERIED_MS - recentSpanMs,
            utilizationPercent: currentUtilization - sustainableRate * (2 / 24),
          },
        ],
      }),
    );

    expect(result?.recentProjectedUtilizationAtReset).toBeCloseTo(100);
    expect(result?.recentExhaustion).toMatchObject({
      kind: "at",
      withinCycle: true,
    });
    expect(result?.recentExhaustion.atMs).toBeCloseTo(RESET_MS, -2);
  });
});

describe("forecastCodexCycle recent forecast", () => {
  it("uses the longest valid span within the most recent 24 hours", () => {
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: 40,
        samples: [
          {
            timestampMs: QUERIED_MS - 30 * HOUR_MS,
            utilizationPercent: 10,
          },
          {
            timestampMs: QUERIED_MS - 20 * HOUR_MS,
            utilizationPercent: 20,
          },
          {
            timestampMs: QUERIED_MS - 5 * HOUR_MS,
            utilizationPercent: 35,
          },
        ],
      }),
    );

    expect(result?.recentWindowStartMs).toBe(QUERIED_MS - 20 * HOUR_MS);
    expect(result?.recentWindowEndMs).toBe(QUERIED_MS);
    expect(result?.recentWindowSpanMs).toBe(20 * HOUR_MS);
    expect(result?.recentRatePercentPerDay).toBeCloseTo(24);
    expect(result?.recentProjectedUtilizationAtReset).toBe(160);
    expect(result?.recentExhaustion).toMatchObject({
      kind: "at",
      withinCycle: true,
    });
  });

  it("uses only the final monotonic segment after utilization regresses", () => {
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: 35,
        samples: [
          {
            timestampMs: QUERIED_MS - 20 * HOUR_MS,
            utilizationPercent: 40,
          },
          {
            timestampMs: QUERIED_MS - 6 * HOUR_MS,
            utilizationPercent: 25,
          },
          {
            timestampMs: QUERIED_MS - 2 * HOUR_MS,
            utilizationPercent: 30,
          },
        ],
      }),
    );

    expect(result?.recentWindowStartMs).toBe(QUERIED_MS - 6 * HOUR_MS);
    expect(result?.recentRatePercentPerDay).toBeCloseTo(40);
    expect(result?.recentUnavailableReason).toBeNull();
  });

  it("marks recent forecast unavailable when a regression ends at current", () => {
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: 30,
        samples: [
          {
            timestampMs: QUERIED_MS - 2 * HOUR_MS,
            utilizationPercent: 40,
          },
        ],
      }),
    );

    expect(result?.recentRatePercentPerDay).toBeNull();
    expect(result?.recentUnavailableReason).toBe("utilization_regression");
    expect(result?.recentProjectedUtilizationAtReset).toBeNull();
    expect(result?.recentExhaustion).toEqual({
      kind: "unavailable",
      atMs: null,
      withinCycle: null,
    });
  });

  it("accepts zero recent growth and reports infinite depletion", () => {
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: 30,
        samples: [
          {
            timestampMs: QUERIED_MS - 2 * HOUR_MS,
            utilizationPercent: 30,
          },
        ],
      }),
    );

    expect(result?.recentRatePercentPerDay).toBe(0);
    expect(result?.recentProjectedUtilizationAtReset).toBe(30);
    expect(result?.recentExhaustion).toEqual({
      kind: "never",
      atMs: null,
      withinCycle: false,
    });
  });

  it.each([
    ["no history", []],
    [
      "less than one hour",
      [
        {
          timestampMs: QUERIED_MS - 30 * 60 * 1000,
          utilizationPercent: 29,
        },
      ],
    ],
    [
      "only older than 24 hours",
      [
        {
          timestampMs: QUERIED_MS - 25 * HOUR_MS,
          utilizationPercent: 20,
        },
      ],
    ],
  ])("marks recent forecast unavailable with %s", (_label, samples) => {
    const result = forecastCodexCycle(makeInput({ samples }));

    expect(result?.recentRatePercentPerDay).toBeNull();
    expect(result?.recentProjectedUtilizationAtReset).toBeNull();
    expect(result?.recentExhaustion.kind).toBe("unavailable");
  });

  it("sorts samples and ignores invalid clocks and out-of-cycle points", () => {
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: 40,
        samples: [
          { timestampMs: Number.NaN, utilizationPercent: 1 },
          { timestampMs: QUERIED_MS + HOUR_MS, utilizationPercent: 99 },
          {
            timestampMs: QUERIED_MS - 2 * HOUR_MS,
            utilizationPercent: 36,
          },
          {
            timestampMs: QUERIED_MS - 5 * HOUR_MS,
            utilizationPercent: 30,
          },
          { timestampMs: START_MS - HOUR_MS, utilizationPercent: 0 },
        ],
      }),
    );

    expect(result?.recentWindowStartMs).toBe(QUERIED_MS - 5 * HOUR_MS);
    expect(result?.recentRatePercentPerDay).toBeCloseTo(48);
  });
});

describe("forecastCodexCycle boundaries", () => {
  it("caps current utilization but preserves raw projections above 100 percent", () => {
    const result = forecastCodexCycle(
      makeInput({
        utilizationPercent: 125,
        samples: [
          {
            timestampMs: QUERIED_MS - 2 * HOUR_MS,
            utilizationPercent: 95,
          },
        ],
      }),
    );

    expect(result?.currentUtilizationPercent).toBe(100);
    expect(result?.cumulativeProjectedUtilizationAtReset).toBe(350);
    expect(result?.recentProjectedUtilizationAtReset).toBeCloseTo(400);
    expect(result?.cumulativeExhaustion).toEqual({
      kind: "at",
      atMs: QUERIED_MS,
      withinCycle: true,
    });
    expect(result?.statusLevel).toBe("exhausted");
  });

  it.each([
    ["non-finite start", { cycleStartMs: Number.NaN }],
    ["reversed cycle", { resetAtMs: START_MS }],
    ["query before cycle", { queriedAtMs: START_MS - 1 }],
    ["query at reset", { queriedAtMs: RESET_MS }],
    ["negative utilization", { utilizationPercent: -1 }],
  ])("rejects %s", (_label, overrides) => {
    expect(forecastCodexCycle(makeInput(overrides))).toBeNull();
  });

  it.each([
    [0.84, "below_pace"],
    [0.85, "on_pace"],
    [1.15, "on_pace"],
    [1.2, "above_pace"],
    [1.35, "above_pace"],
    [1.36, "far_above_pace"],
  ] as const)(
    "derives status only from the current-to-baseline pace ratio %s",
    (paceRatio, expectedStatus) => {
      const baseline = (2 / 7) * 100;
      const result = forecastCodexCycle(
        makeInput({
          utilizationPercent: baseline * paceRatio,
          // Deliberately extreme recent growth: it must not alter pace status.
          samples: [
            {
              timestampMs: QUERIED_MS - 2 * HOUR_MS,
              utilizationPercent: 0,
            },
          ],
        }),
      );

      expect(result?.paceRatio).toBeCloseTo(paceRatio);
      expect(result?.statusLevel).toBe(expectedStatus);
    },
  );
});
