import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import en from "./locales/en.json";
import ja from "./locales/ja.json";
import zh from "./locales/zh.json";
import zhTW from "./locales/zh-TW.json";

type Language = "zh" | "zh-TW" | "en" | "ja";

const DEFAULT_LANGUAGE: Language = "zh";

const getInitialLanguage = (): Language => {
  if (typeof window !== "undefined") {
    try {
      const stored = window.localStorage.getItem("language");
      if (
        stored === "zh" ||
        stored === "zh-TW" ||
        stored === "en" ||
        stored === "ja"
      ) {
        return stored;
      }
    } catch (error) {
      console.warn("[i18n] Failed to read stored language preference", error);
    }
  }

  const navigatorLang =
    typeof navigator !== "undefined"
      ? (navigator.language?.toLowerCase() ??
        navigator.languages?.[0]?.toLowerCase())
      : undefined;

  if (navigatorLang === "zh") {
    return "zh";
  }

  if (
    navigatorLang?.startsWith("zh-tw") ||
    navigatorLang?.startsWith("zh-hk") ||
    navigatorLang?.startsWith("zh-mo") ||
    navigatorLang?.startsWith("zh-hant")
  ) {
    return "zh-TW";
  }

  if (navigatorLang?.startsWith("zh")) {
    return "zh";
  }

  if (navigatorLang?.startsWith("ja")) {
    return "ja";
  }

  if (navigatorLang?.startsWith("en")) {
    return "en";
  }

  return DEFAULT_LANGUAGE;
};

export function withOmpAliases<T>(value: T): T {
  const visit = (current: unknown, ompAlias: boolean): unknown => {
    if (typeof current === "string") {
      return ompAlias ? current.split("Pi").join("OMP") : current;
    }
    if (Array.isArray(current)) {
      return current.map((child) => visit(child, ompAlias));
    }
    if (!current || typeof current !== "object") {
      return current;
    }

    const source = current as Record<string, unknown>;
    const result = Object.fromEntries(
      Object.entries(source).map(([key, child]) => [
        key,
        visit(child, ompAlias),
      ]),
    ) as Record<string, unknown>;
    for (const [key, child] of Object.entries(source)) {
      const alias =
        key === "pi"
          ? "omp"
          : /^pi[A-Z]/.test(key)
            ? `omp${key.slice(2)}`
            : key.includes("Pi")
              ? key.split("Pi").join("Omp")
              : undefined;
      if (alias && !(alias in source)) {
        result[alias] = visit(child, true);
      }
    }
    return result;
  };

  return visit(value, false) as T;
}

const resources = {
  en: {
    translation: withOmpAliases(en),
  },
  ja: {
    translation: withOmpAliases(ja),
  },
  zh: {
    translation: withOmpAliases(zh),
  },
  "zh-TW": {
    translation: withOmpAliases(zhTW),
  },
};

i18n.use(initReactI18next).init({
  resources,
  lng: getInitialLanguage(), // 根据本地存储或系统语言选择默认语言
  fallbackLng: "en", // 如果缺少中文翻译则退回英文

  interpolation: {
    escapeValue: false, // React 已经默认转义
  },

  // 开发模式下显示调试信息
  debug: false,
});

export default i18n;
