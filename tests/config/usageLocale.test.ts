import { describe, expect, it } from "vitest";
import en from "@/i18n/locales/en.json";
import ja from "@/i18n/locales/ja.json";
import zhTW from "@/i18n/locales/zh-TW.json";
import zh from "@/i18n/locales/zh.json";

describe("usage token labels", () => {
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
});
