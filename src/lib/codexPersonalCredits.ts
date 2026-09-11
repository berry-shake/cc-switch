import type { CodexQuotaCycleWindow } from "@/lib/codexCycleCapacity";
import type {
  CodexPersonalCredits,
  PersonalCreditModel,
} from "@/types/subscription";

const DAY_MS = 86_400_000;
export const DEFAULT_CREDITS_PER_USD = 25;
export const CREDITS_PER_USD_STORAGE_KEY = "cc-switch:codex-credits-per-usd:v1";

export function parseCreditsPerUsd(value: string | number): number | null {
  if (typeof value === "string" && !value.trim()) return null;
  const n = Number(value);
  return Number.isFinite(n) && n > 0 && n <= 1e9 ? n : null;
}

export function readCreditsPerUsd(): number {
  try {
    const value = localStorage.getItem(CREDITS_PER_USD_STORAGE_KEY);
    return value == null
      ? DEFAULT_CREDITS_PER_USD
      : (parseCreditsPerUsd(value) ?? DEFAULT_CREDITS_PER_USD);
  } catch {
    return DEFAULT_CREDITS_PER_USD;
  }
}

export function persistCreditsPerUsd(value: string): void {
  const rate = parseCreditsPerUsd(value);
  if (rate == null) return;
  try {
    localStorage.setItem(CREDITS_PER_USD_STORAGE_KEY, String(rate));
  } catch {
    /* Auxiliary preference only. */
  }
}

function valid(value: number | null): value is number {
  return value != null && Number.isFinite(value) && value >= 0;
}

function sumKnown(values: (number | null)[]): number | null {
  const known = values.filter(valid);
  const sum = known.reduce((a, b) => a + b, 0);
  return known.length > 0 && Number.isFinite(sum) ? sum : null;
}

/** Credits and tokens remain independent. Missing data blocks only its own
 * capacity projection, never a known total or the quota-based pace forecast.
 * Daily buckets overlap the precise cycle; we disclose rather than prorate them.
 */
export function summarizePersonalCredits(
  data: CodexPersonalCredits,
  cycle: CodexQuotaCycleWindow | null,
  creditsPerUsd: number | null,
) {
  if (!cycle) return null;
  const endMs = Math.min(cycle.endMs, cycle.resetAtMs);
  const dates = new Set<string>();
  const days = data.days.filter((day) => {
    const ms = Date.parse(`${day.date}T00:00:00Z`);
    return (
      Number.isFinite(ms) &&
      new Date(ms).toISOString().slice(0, 10) === day.date &&
      ms < endMs &&
      ms + DAY_MS > cycle.startMs
    );
  });
  // Defend against malformed IPC/cache fixtures as well as server duplicates.
  for (const day of days) {
    if (dates.has(day.date)) return null;
    dates.add(day.date);
  }
  const totalsAvailable = data.totalsStatus === "available";
  const creditValues = days.map((day) =>
    totalsAvailable ? day.credits : null,
  );
  const tokenValues = days.map((day) =>
    totalsAvailable ? day.tokens.totalTokens : null,
  );
  const credits = sumKnown(creditValues);
  const tokens = sumKnown(tokenValues);
  const missingCreditDates = days
    .filter((_, i) => !valid(creditValues[i]))
    .map((day) => day.date);
  const missingTokenDates = days
    .filter((_, i) => !valid(tokenValues[i]))
    .map((day) => day.date);
  const ratio =
    cycle.utilizationPercent != null &&
    cycle.utilizationPercent > 0 &&
    cycle.utilizationPercent <= 100
      ? cycle.utilizationPercent / 100
      : null;
  const project = (value: number | null, missing: string[]) => {
    if (value == null || ratio == null || missing.length > 0) return null;
    const result = value / ratio;
    return Number.isFinite(result) ? result : null;
  };
  const rate = creditsPerUsd == null ? null : parseCreditsPerUsd(creditsPerUsd);
  const usd = (value: number | null) => {
    if (value == null || rate == null) return null;
    const result = value / rate;
    return Number.isFinite(result) ? result : null;
  };
  const totalCredits = project(credits, missingCreditDates);
  const totalTokens = project(tokens, missingTokenDates);
  const remainingCredits =
    totalCredits != null && credits != null
      ? Math.max(0, totalCredits - credits)
      : null;
  const models = new Map<string, PersonalCreditModel>();
  if (totalsAvailable && data.breakdownStatus === "available") {
    for (const day of days) {
      for (const model of day.models) {
        if (!valid(model.credits)) continue;
        const key = JSON.stringify([model.model, model.speed]);
        const existing = models.get(key);
        models.set(key, {
          ...model,
          credits: (existing?.credits ?? 0) + model.credits,
        });
      }
    }
  }
  return {
    days,
    credits,
    tokens,
    usedUsd: usd(credits),
    totalCredits,
    remainingCredits,
    totalUsd: usd(totalCredits),
    remainingUsd: usd(remainingCredits),
    totalTokens,
    remainingTokens:
      totalTokens != null && tokens != null
        ? Math.max(0, totalTokens - tokens)
        : null,
    missingCreditDates,
    missingTokenDates,
    unallocatedCredits: totalsAvailable
      ? sumKnown(days.map((day) => day.unallocatedCredits))
      : null,
    models: [...models.values()]
      .filter((model) => Number.isFinite(model.credits))
      .sort((a, b) => b.credits - a.credits),
    boundaryDates: days
      .filter((day) => {
        const start = Date.parse(`${day.date}T00:00:00Z`);
        return start < cycle.startMs || start + DAY_MS > cycle.resetAtMs;
      })
      .map((day) => day.date),
  };
}
