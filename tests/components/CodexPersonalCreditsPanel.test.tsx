import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { CodexPersonalCreditsPanel } from "@/components/usage/CodexPersonalCreditsPanel";
import type { CodexPersonalCredits } from "@/types/subscription";
import { CREDITS_PER_USD_STORAGE_KEY } from "@/lib/codexPersonalCredits";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, fallback: string) => fallback ?? key,
  }),
}));
const cycle = {
  startMs: Date.UTC(2026, 8, 9),
  endMs: Date.UTC(2026, 8, 11),
  resetAtMs: Date.UTC(2026, 8, 16),
  windowSeconds: 604800,
  utilizationPercent: 20,
};
const data: CodexPersonalCredits = {
  totalsStatus: "available",
  breakdownStatus: "available",
  days: [
    {
      date: "2026-09-10",
      credits: 1250,
      unallocatedCredits: 0,
      allocation: "allocated",
      models: [{ model: "gpt-6-astra", speed: "fast", credits: 1250 }],
      tokens: {
        totalTokens: 1000,
        uncachedInputTokens: 100,
        cachedInputTokens: 800,
        outputTokens: 100,
        cacheWriteInputTokens: 0,
      },
    },
  ],
};
const metric = (label: string) => screen.getByText(label).parentElement!;
describe("CodexPersonalCreditsPanel", () => {
  beforeEach(() => localStorage.clear());
  it("renders raw values without a price table and allows custom USD conversion", () => {
    render(<CodexPersonalCreditsPanel data={data} cycle={cycle} />);
    expect(
      within(metric("已用 Credits（接口原始）")).getByText("1,250"),
    ).toBeInTheDocument();
    expect(
      within(metric("已同步 Token（日桶合计）")).getByText("1K"),
    ).toBeInTheDocument();
    expect(
      within(metric("已用美元等值（估算）")).getByText("$50.00"),
    ).toBeInTheDocument();
    const input = screen.getByRole("spinbutton");
    fireEvent.change(input, { target: { value: "50" } });
    fireEvent.blur(input);
    expect(
      within(metric("已用美元等值（估算）")).getByText("$25.00"),
    ).toBeInTheDocument();
    expect(localStorage.getItem(CREDITS_PER_USD_STORAGE_KEY)).toBe("50");
    expect(
      within(metric("已用 Credits（接口原始）")).getByText("1,250"),
    ).toBeInTheDocument();
  });
  it("invalid conversion cannot turn into zero dollars", () => {
    render(<CodexPersonalCreditsPanel data={data} cycle={cycle} />);
    fireEvent.change(screen.getByRole("spinbutton"), {
      target: { value: "0" },
    });
    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(
      within(metric("已用美元等值（估算）")).getByText("待同步 / 待估算"),
    ).toBeInTheDocument();
    expect(
      within(metric("已用 Credits（接口原始）")).getByText("1,250"),
    ).toBeInTheDocument();
  });
  it("preserves totals when model endpoint fails", () => {
    render(
      <CodexPersonalCreditsPanel
        data={{ ...data, breakdownStatus: "unavailable" }}
        cycle={cycle}
      />,
    );
    expect(screen.getByText(/模型明细缺失或校验失败/)).toBeInTheDocument();
    expect(
      within(metric("100% 周期美元等值（估算）")).getByText("$250.00"),
    ).toBeInTheDocument();
  });
  it("unknown or invalid data remains visibly unavailable", () => {
    render(
      <CodexPersonalCreditsPanel
        data={{ ...data, totalsStatus: "invalid", days: [] }}
        cycle={cycle}
      />,
    );
    expect(screen.getByTestId("credits-totals-warning")).toHaveTextContent(
      "校验失败",
    );
    expect(
      within(metric("已用 Credits（接口原始）")).getByText("待同步 / 待估算"),
    ).toBeInTheDocument();
  });
});
