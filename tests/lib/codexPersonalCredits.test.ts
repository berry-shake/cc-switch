import { beforeEach, describe, expect, it } from "vitest";
import {
  CREDITS_PER_USD_STORAGE_KEY,
  parseCreditsPerUsd,
  persistCreditsPerUsd,
  readCreditsPerUsd,
  summarizePersonalCredits,
} from "@/lib/codexPersonalCredits";
import type { CodexPersonalCredits } from "@/types/subscription";
import type { CodexQuotaCycleWindow } from "@/lib/codexCycleCapacity";

const dayMs = 86_400_000;
const startMs = Date.UTC(2026, 8, 9);
const cycle: CodexQuotaCycleWindow = {
  startMs,
  endMs: startMs + 2.5 * dayMs,
  resetAtMs: startMs + 7 * dayMs,
  windowSeconds: 604800,
  utilizationPercent: 20,
};
function data(): CodexPersonalCredits {
  return {
    totalsStatus: "available",
    breakdownStatus: "available",
    days: [
      {
        date: "2026-09-10",
        credits: 1250.25,
        allocation: "allocated",
        unallocatedCredits: 0,
        tokens: {
          totalTokens: 1000,
          uncachedInputTokens: 100,
          cachedInputTokens: 800,
          outputTokens: 100,
          cacheWriteInputTokens: 0,
        },
        models: [{ model: "gpt-6-astra", speed: "fast", credits: 1250.25 }],
      },
    ],
  };
}

describe("personal raw Credits", () => {
  beforeEach(() => localStorage.clear());
  it.each(["gpt-6-astra", "gpt-6-sol", "gpt-6-luna"])(
    "uses %s raw credits without another Fast multiplier",
    (model) => {
      const value = data();
      value.days[0].models[0].model = model;
      const result = summarizePersonalCredits(value, cycle, 25)!;
      expect(result.credits).toBe(1250.25);
      expect(result.usedUsd).toBe(50.01);
      expect(result.totalUsd).toBeCloseTo(250.05);
      expect(result.remainingUsd).toBeCloseTo(200.04);
      expect(result.totalTokens).toBe(5000);
      expect(result.models[0].credits).toBe(1250.25);
    },
  );
  it("keeps Credits and USD even when all tokens are missing", () => {
    const value = data();
    value.days[0].tokens.totalTokens = null;
    const result = summarizePersonalCredits(value, cycle, 25)!;
    expect(result.tokens).toBeNull();
    expect(result.totalTokens).toBeNull();
    expect(result.totalUsd).toBeCloseTo(250.05);
  });
  it("keeps tokens when credits are missing and does not fall back to token pricing", () => {
    const value = data();
    value.days[0].credits = null;
    const result = summarizePersonalCredits(value, cycle, 25)!;
    expect(result.credits).toBeNull();
    expect(result.usedUsd).toBeNull();
    expect(result.totalTokens).toBe(5000);
  });
  it("shows known partial totals but blocks incomplete Credits capacity", () => {
    const value = data();
    value.days.push({
      ...value.days[0],
      date: "2026-09-11",
      credits: null,
      models: [],
      unallocatedCredits: null,
      allocation: "pending",
    });
    const result = summarizePersonalCredits(value, cycle, 25)!;
    expect(result.credits).toBe(1250.25);
    expect(result.usedUsd).toBe(50.01);
    expect(result.totalCredits).toBeNull();
    expect(result.missingCreditDates).toEqual(["2026-09-11"]);
    expect(result.totalTokens).toBe(10000);
  });
  it("missing model details do not block raw credits capacity", () => {
    const value = data();
    value.breakdownStatus = "unavailable";
    value.days[0].models = [];
    value.days[0].unallocatedCredits = 1250.25;
    const result = summarizePersonalCredits(value, cycle, 25)!;
    expect(result.totalUsd).toBeCloseTo(250.05);
    expect(result.unallocatedCredits).toBe(1250.25);
    expect(result.models).toEqual([]);
  });
  it.each([0, null])(
    "does not extrapolate from missing or zero utilization: %s",
    (utilizationPercent) => {
      const result = summarizePersonalCredits(
        data(),
        { ...cycle, utilizationPercent },
        25,
      )!;
      expect(result.usedUsd).toBe(50.01);
      expect(result.totalCredits).toBeNull();
      expect(result.totalTokens).toBeNull();
    },
  );
  it("preserves true zero and refuses invalid totals", () => {
    const value = data();
    value.days[0].credits = 0;
    value.days[0].tokens.totalTokens = 0;
    const result = summarizePersonalCredits(value, cycle, 25)!;
    expect(result.credits).toBe(0);
    expect(result.tokens).toBe(0);
    expect(result.totalUsd).toBe(0);
    value.totalsStatus = "invalid";
    expect(summarizePersonalCredits(value, cycle, 25)?.credits).toBeNull();
  });
  it("filters exact UTC day overlap and marks boundary days without prorating", () => {
    const value = data();
    value.days.push(
      { ...value.days[0], date: "2026-09-08" },
      { ...value.days[0], date: "2026-09-12" },
    );
    const result = summarizePersonalCredits(
      value,
      { ...cycle, startMs: Date.UTC(2026, 8, 10, 12) },
      25,
    )!;
    expect(result.days.map((day) => day.date)).toEqual(["2026-09-10"]);
    expect(result.boundaryDates).toEqual(["2026-09-10"]);
    expect(result.credits).toBe(1250.25);
  });
  it("refuses duplicate days and missing cycle", () => {
    const value = data();
    value.days.push({ ...value.days[0] });
    expect(summarizePersonalCredits(value, cycle, 25)).toBeNull();
    expect(summarizePersonalCredits(data(), null, 25)).toBeNull();
  });
  it("does not convert invalid rates and safely persists valid preferences", () => {
    for (const value of ["", "0", "-1", "NaN", "Infinity", "1000000001"])
      expect(parseCreditsPerUsd(value)).toBeNull();
    expect(readCreditsPerUsd()).toBe(25);
    persistCreditsPerUsd("50");
    expect(readCreditsPerUsd()).toBe(50);
    persistCreditsPerUsd("0");
    expect(readCreditsPerUsd()).toBe(50);
    localStorage.setItem(CREDITS_PER_USD_STORAGE_KEY, "broken");
    expect(readCreditsPerUsd()).toBe(25);
    expect(summarizePersonalCredits(data(), cycle, null)?.usedUsd).toBeNull();
    expect(summarizePersonalCredits(data(), cycle, 50)?.usedUsd).toBe(25.005);
  });
});
