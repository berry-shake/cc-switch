import { describe, expect, it } from "vitest";
import en from "@/i18n/locales/en.json";
import ja from "@/i18n/locales/ja.json";
import zhTW from "@/i18n/locales/zh-TW.json";
import zh from "@/i18n/locales/zh.json";

function flattenTranslationStrings(
  value: unknown,
  path: string[] = [],
  result = new Map<string, string>(),
): Map<string, string> {
  if (typeof value === "string") {
    result.set(path.join("."), value);
  } else if (value && typeof value === "object") {
    for (const [key, child] of Object.entries(value)) {
      flattenTranslationStrings(child, [...path, key], result);
    }
  }
  return result;
}

function interpolationVariables(value: string): string[] {
  return Array.from(
    value.matchAll(/\{\{\s*([^}]+?)\s*\}\}/g),
    ([, name]) => name,
  ).sort();
}

describe("usage token labels", () => {
  it("keeps personal Credits translations aligned in all languages", () => {
    const reference = flattenTranslationStrings(en.usage.personalCredits);
    for (const locale of [zh, zhTW, ja]) {
      const actual = flattenTranslationStrings(locale.usage.personalCredits);
      expect([...actual.keys()].sort()).toEqual([...reference.keys()].sort());
    }
  });
  it.each([
    [en, ["Input", "Cache Write", "Cache Read", "Output"]],
    [zh, ["输入", "缓存写入", "缓存读取", "输出"]],
    [zhTW, ["輸入", "快取寫入", "快取讀取", "輸出"]],
    [ja, ["入力", "キャッシュ書き込み", "キャッシュ読み取り", "出力"]],
  ])(
    "keeps the four billing token labels in canonical order",
    (locale, expected) => {
      expect([
        locale.usage.inputTokens,
        locale.usage.cacheCreationTokens,
        locale.usage.cacheReadTokens,
        locale.usage.outputTokens,
      ]).toEqual(expected);
    },
  );

  it("keeps simplified Chinese short and cost labels consistent", () => {
    expect([
      zh.usage.input,
      zh.usage.cacheWrite,
      zh.usage.cacheRead,
      zh.usage.output,
    ]).toEqual(["输入", "缓存写入", "缓存读取", "输出"]);
    expect([
      zh.usage.inputCost,
      zh.usage.cacheWriteCost,
      zh.usage.cacheReadCost,
      zh.usage.outputCost,
    ]).toEqual(["输入成本", "缓存写入成本", "缓存读取成本", "输出成本"]);
  });

  it("keeps Chinese used labels concise", () => {
    expect([
      zh.usage.used,
      zh.usage.cycleCapacity.used,
      zh.usage.cycleCapacity.usedTokens,
      zh.usage.cycleCapacity.usedUsd,
    ]).toEqual([
      "已用：",
      "已用",
      "已用额度 Token 等效容量",
      "已用额度美元等效容量",
    ]);
    expect([
      zhTW.usage.used,
      zhTW.usage.cycleCapacity.used,
      zhTW.usage.cycleCapacity.usedTokens,
      zhTW.usage.cycleCapacity.usedUsd,
    ]).toEqual([
      "已用：",
      "已用",
      "已用額度 Token 等效容量",
      "已用額度美元等效容量",
    ]);
  });

  it("keeps Codex cycle forecast keys and interpolation variables aligned", () => {
    const reference = flattenTranslationStrings(en.usage.cycleCapacity);

    for (const locale of [zh, zhTW, ja]) {
      const actual = flattenTranslationStrings(locale.usage.cycleCapacity);
      expect([...actual.keys()].sort()).toEqual([...reference.keys()].sort());

      for (const [key, template] of reference) {
        expect(interpolationVariables(actual.get(key) ?? "")).toEqual(
          interpolationVariables(template),
        );
      }
    }
  });
});
