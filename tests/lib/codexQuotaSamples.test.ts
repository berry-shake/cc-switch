import { beforeEach, describe, expect, it } from "vitest";

import {
  codexQuotaSampleStorageKey,
  getCodexQuotaSamplesForCycle,
  loadCodexQuotaSamples,
  mergeCodexQuotaSample,
  recordCodexQuotaSample,
  type CodexQuotaSample,
} from "@/lib/codexQuotaSamples";

const DAY_MS = 24 * 60 * 60 * 1000;
const NOW = Date.UTC(2026, 7, 22, 8);

function sample(
  capturedAtMs: number,
  utilizationPercent = 30,
): CodexQuotaSample {
  return {
    capturedAtMs,
    cycleStartMs: NOW - 2 * DAY_MS,
    resetAtMs: NOW + 5 * DAY_MS,
    windowSeconds: 7 * 24 * 60 * 60,
    utilizationPercent,
  };
}

describe("Codex quota sample storage", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("round-trips valid samples and ignores corrupt storage", () => {
    recordCodexQuotaSample(sample(NOW));
    expect(loadCodexQuotaSamples()).toEqual([sample(NOW)]);

    localStorage.setItem(codexQuotaSampleStorageKey, "not-json");
    expect(loadCodexQuotaSamples()).toEqual([]);
  });

  it("replaces duplicate server timestamps and keeps chronological order", () => {
    const merged = mergeCodexQuotaSample(
      [sample(NOW - 2 * 60_000, 20), sample(NOW, 29)],
      sample(NOW, 30),
    );

    expect(merged).toEqual([sample(NOW - 2 * 60_000, 20), sample(NOW, 30)]);
  });

  it("drops invalid and older-than-21-day samples", () => {
    const stale = {
      ...sample(NOW - 22 * DAY_MS),
      cycleStartMs: NOW - 30 * DAY_MS,
      resetAtMs: NOW - 20 * DAY_MS,
    };
    const invalid = { ...sample(NOW - DAY_MS), utilizationPercent: 120 };

    expect(
      mergeCodexQuotaSample(
        [stale, invalid] as CodexQuotaSample[],
        sample(NOW),
        NOW,
      ),
    ).toEqual([sample(NOW)]);
  });

  it("selects only snapshots from the exact current cycle and window", () => {
    const current = sample(NOW - 2 * 60_000, 29);
    const wrongReset = {
      ...sample(NOW - 60_000, 99),
      resetAtMs: NOW + 6 * DAY_MS,
    };
    const wrongWindow = {
      ...sample(NOW - 30_000, 98),
      windowSeconds: 30 * 24 * 60 * 60,
    };
    const future = sample(NOW + 60_000, 31);

    expect(
      getCodexQuotaSamplesForCycle([wrongReset, future, current, wrongWindow], {
        startMs: current.cycleStartMs,
        resetAtMs: current.resetAtMs,
        windowSeconds: current.windowSeconds,
        endMs: NOW,
      }),
    ).toEqual([current]);
  });

  it("caps the rolling cache at 1024 chronological samples", () => {
    const samples = Array.from({ length: 1_200 }, (_, index) =>
      sample(NOW - (1_200 - index) * 60_000, 20 + index / 100),
    );
    const merged = mergeCodexQuotaSample(samples, sample(NOW, 30), NOW);

    expect(merged).toHaveLength(1_024);
    expect(merged.at(-1)).toEqual(sample(NOW, 30));
    expect(merged[0].capturedAtMs).toBe(NOW - 1_023 * 60_000);
  });

  it.each([
    ["cycle start", { capturedAtMs: NOW - 2 * DAY_MS }],
    ["cycle reset", { capturedAtMs: NOW + 5 * DAY_MS }],
    ["NaN utilization", { utilizationPercent: Number.NaN }],
    ["infinite timestamp", { capturedAtMs: Number.POSITIVE_INFINITY }],
  ])("rejects a sample at an invalid %s boundary", (_label, overrides) => {
    expect(
      mergeCodexQuotaSample([], { ...sample(NOW), ...overrides }, NOW),
    ).toEqual([]);
  });

  it("keeps the in-memory result when browser storage is not writable", () => {
    const storage = {
      getItem: () => null,
      setItem: () => {
        throw new Error("quota exceeded");
      },
    } as unknown as Storage;

    expect(recordCodexQuotaSample(sample(NOW), storage)).toEqual([sample(NOW)]);
  });
});
