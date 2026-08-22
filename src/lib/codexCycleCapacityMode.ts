export const CODEX_CYCLE_CAPACITY_EXPANDED_STORAGE_KEY =
  "cc-switch.usage.codexCycleCapacity.expanded";
export const CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY =
  "cc-switch.usage.codexCycleCapacity.calculationMode";

export type CodexCycleCapacityCalculationMode = "local" | "analytics";

export function readCodexCycleCapacityMode(): CodexCycleCapacityCalculationMode {
  if (typeof window === "undefined") return "local";

  try {
    const stored = window.localStorage.getItem(
      CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY,
    );
    if (stored === "analytics") return "analytics";
  } catch {
    // localStorage may be unavailable in restricted webviews; keep the default.
  }

  return "local";
}

export function persistCodexCycleCapacityMode(
  mode: CodexCycleCapacityCalculationMode,
): void {
  if (typeof window === "undefined") return;

  try {
    window.localStorage.setItem(CODEX_CYCLE_CAPACITY_MODE_STORAGE_KEY, mode);
  } catch {
    // The switch remains usable even when the preference cannot be persisted.
  }
}
