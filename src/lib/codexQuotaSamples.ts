import type { CodexQuotaCycle } from "@/lib/codexCycleCapacity";

const STORAGE_KEY = "cc-switch:codex-quota-samples:v1";
const DAY_MS = 24 * 60 * 60 * 1000;
const MAX_SAMPLE_AGE_MS = 21 * DAY_MS;
const MAX_SAMPLES = 1024;

export interface CodexQuotaSample {
  capturedAtMs: number;
  cycleStartMs: number;
  resetAtMs: number;
  windowSeconds: number;
  utilizationPercent: number;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isValidSample(value: unknown): value is CodexQuotaSample {
  if (!value || typeof value !== "object") return false;
  const sample = value as Partial<CodexQuotaSample>;
  return (
    isFiniteNumber(sample.capturedAtMs) &&
    isFiniteNumber(sample.cycleStartMs) &&
    isFiniteNumber(sample.resetAtMs) &&
    isFiniteNumber(sample.windowSeconds) &&
    isFiniteNumber(sample.utilizationPercent) &&
    sample.windowSeconds > 0 &&
    sample.cycleStartMs < sample.capturedAtMs &&
    sample.capturedAtMs < sample.resetAtMs &&
    sample.utilizationPercent >= 0 &&
    sample.utilizationPercent <= 100
  );
}

function resolveStorage(storage?: Storage | null): Storage | null {
  if (storage !== undefined) return storage;
  if (typeof window === "undefined") return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/**
 * 读取本机额度采样缓存。损坏、旧格式或不可访问的存储都按空缓存处理；
 * 该缓存仅用于近期趋势，不应影响周期容量主入口的可用性。
 */
export function loadCodexQuotaSamples(
  storage?: Storage | null,
): CodexQuotaSample[] {
  const target = resolveStorage(storage);
  if (!target) return [];

  try {
    const raw = target.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.filter(isValidSample).sort((a, b) => {
      return a.capturedAtMs - b.capturedAtMs;
    });
  } catch {
    return [];
  }
}

export function sampleCodexQuotaCycle(
  cycle: CodexQuotaCycle,
): CodexQuotaSample {
  return {
    capturedAtMs: cycle.endMs,
    cycleStartMs: cycle.startMs,
    resetAtMs: cycle.resetAtMs,
    windowSeconds: cycle.windowSeconds,
    utilizationPercent: cycle.utilizationPercent,
  };
}

/** 只返回与当前额度周期严格一致的快照，避免跨周期或窗口混算近期速率。 */
export function getCodexQuotaSamplesForCycle(
  samples: readonly CodexQuotaSample[],
  cycle: Pick<
    CodexQuotaCycle,
    "startMs" | "resetAtMs" | "windowSeconds" | "endMs"
  >,
): CodexQuotaSample[] {
  return samples
    .filter(isValidSample)
    .filter(
      (sample) =>
        sample.cycleStartMs === cycle.startMs &&
        sample.resetAtMs === cycle.resetAtMs &&
        sample.windowSeconds === cycle.windowSeconds &&
        sample.capturedAtMs <= cycle.endMs,
    )
    .sort((a, b) => a.capturedAtMs - b.capturedAtMs);
}

/**
 * 合并一次成功额度快照。相同服务端采样时刻会被替换，避免 React StrictMode
 * 或页面重复挂载产生重复点；缓存只保留最近 21 天和最多 1024 个点，
 * 即使按 5 分钟轮询也能覆盖超过 48 小时。
 */
export function mergeCodexQuotaSample(
  samples: readonly CodexQuotaSample[],
  incoming: CodexQuotaSample,
  nowMs: number = incoming.capturedAtMs,
): CodexQuotaSample[] {
  if (!isValidSample(incoming) || !Number.isFinite(nowMs)) {
    return samples.filter(isValidSample);
  }

  const cutoffMs = nowMs - MAX_SAMPLE_AGE_MS;
  const merged = samples
    .filter(isValidSample)
    .filter((sample) => sample.capturedAtMs >= cutoffMs)
    .filter((sample) => sample.capturedAtMs !== incoming.capturedAtMs);
  merged.push(incoming);
  merged.sort((a, b) => a.capturedAtMs - b.capturedAtMs);
  return merged.slice(-MAX_SAMPLES);
}

/**
 * 写入本机额度采样缓存并返回最终序列。localStorage 被禁用或写满时静默回退，
 * 调用方仍可用当前快照完成累计预测。
 */
export function recordCodexQuotaSample(
  incoming: CodexQuotaSample,
  storage?: Storage | null,
): CodexQuotaSample[] {
  const target = resolveStorage(storage);
  const merged = mergeCodexQuotaSample(loadCodexQuotaSamples(target), incoming);
  if (!target) return merged;

  try {
    target.setItem(STORAGE_KEY, JSON.stringify(merged));
  } catch {
    // 近期采样是可重建的辅助数据；存储不可写不应让主卡片失败。
  }
  return merged;
}

export const codexQuotaSampleStorageKey = STORAGE_KEY;
