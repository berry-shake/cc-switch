import { describe, expect, it } from "vitest";
import {
  getCacheWriteAvailability,
  getFreshInputTokens,
  INPUT_TOKEN_SEMANTICS_FRESH,
  INPUT_TOKEN_SEMANTICS_LEGACY,
  INPUT_TOKEN_SEMANTICS_TOTAL,
} from "@/types/usage";

describe("getCacheWriteAvailability", () => {
  it("distinguishes cache-write support across fixed protocols", () => {
    expect(getCacheWriteAvailability(["claude"])).toBe("ok");
    expect(getCacheWriteAvailability(["pi"])).toBe("partial");
    expect(getCacheWriteAvailability(["codex"])).toBe("partial");
    expect(getCacheWriteAvailability(["gemini", "grokbuild"])).toBe("na");
    expect(getCacheWriteAvailability(["codex", "gemini"])).toBe("partial");
    expect(getCacheWriteAvailability(["claude", "codex"])).toBe("partial");
    expect(getCacheWriteAvailability([])).toBe("ok");
  });
});

describe("getFreshInputTokens", () => {
  const codexLog = {
    appType: "codex",
    inputTokens: 100,
    cacheReadTokens: 30,
    cacheCreationTokens: 20,
  };

  it("subtracts cache reads and writes from TOTAL-semantics rows", () => {
    expect(
      getFreshInputTokens({
        ...codexLog,
        inputTokenSemantics: INPUT_TOKEN_SEMANTICS_TOTAL,
      }),
    ).toBe(50);
  });

  it("keeps the legacy read-only deduction for legacy or missing semantics", () => {
    expect(
      getFreshInputTokens({
        ...codexLog,
        inputTokenSemantics: INPUT_TOKEN_SEMANTICS_LEGACY,
      }),
    ).toBe(70);
    expect(getFreshInputTokens(codexLog)).toBe(70);
  });

  it("passes through FRESH-semantics rows", () => {
    expect(
      getFreshInputTokens({
        ...codexLog,
        inputTokenSemantics: INPUT_TOKEN_SEMANTICS_FRESH,
      }),
    ).toBe(100);
  });

  it("fails closed when cache buckets exceed total input", () => {
    expect(
      getFreshInputTokens({
        ...codexLog,
        inputTokens: 40,
        inputTokenSemantics: INPUT_TOKEN_SEMANTICS_TOTAL,
      }),
    ).toBe(40);
  });

  it("passes through non-cache-inclusive apps and unknown semantics", () => {
    expect(
      getFreshInputTokens({
        ...codexLog,
        appType: "claude",
        inputTokenSemantics: INPUT_TOKEN_SEMANTICS_TOTAL,
      }),
    ).toBe(100);
    expect(
      getFreshInputTokens({
        ...codexLog,
        inputTokenSemantics: 99,
      }),
    ).toBe(100);
  });
});
