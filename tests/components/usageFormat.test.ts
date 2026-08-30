import { describe, expect, it } from "vitest";
import {
  formatCompactCount,
  getLocaleFromLanguage,
} from "@/components/usage/format";

describe("usage format helpers", () => {
  it("formats every count with language-independent K, M, and B units", () => {
    expect(formatCompactCount(Number.NaN)).toBe("0");
    expect(formatCompactCount(0)).toBe("0");
    expect(formatCompactCount(999)).toBe("999");
    expect(formatCompactCount(1_000)).toBe("1K");
    expect(formatCompactCount(12_345)).toBe("12.35K");
    expect(formatCompactCount(1_000_000)).toBe("1M");
    expect(formatCompactCount(12_345_678)).toBe("12.35M");
    expect(formatCompactCount(1_234_567_890)).toBe("1.23B");
  });

  it("resolves Traditional Chinese locale aliases", () => {
    expect(getLocaleFromLanguage("zh_TW")).toBe("zh-TW");
    expect(getLocaleFromLanguage("zh-HK")).toBe("zh-TW");
  });
});
