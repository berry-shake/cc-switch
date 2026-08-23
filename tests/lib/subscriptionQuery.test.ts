import { describe, expect, it, vi } from "vitest";

import {
  resetCodexSubscriptionQueries,
  subscriptionKeys,
} from "@/lib/query/subscription";

describe("resetCodexSubscriptionQueries", () => {
  it("clears and refetches every account-sensitive Codex query", async () => {
    const resetQueries = vi.fn().mockResolvedValue(undefined);

    await resetCodexSubscriptionQueries({ resetQueries } as never);

    expect(resetQueries).toHaveBeenCalledTimes(3);
    expect(resetQueries).toHaveBeenCalledWith({
      queryKey: subscriptionKeys.quota("codex"),
      exact: true,
    });
    expect(resetQueries).toHaveBeenCalledWith({
      queryKey: subscriptionKeys.codexQuotaSnapshot(),
      exact: true,
    });
    expect(resetQueries).toHaveBeenCalledWith({
      queryKey: subscriptionKeys.codexOfficialSnapshot(),
      exact: true,
    });
  });
});
