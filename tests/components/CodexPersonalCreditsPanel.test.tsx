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
const rawCredits = () => screen.getByTestId("codex-personal-credits-total");
describe("CodexPersonalCreditsPanel", () => {
  beforeEach(() => localStorage.clear());
  it("keeps the original six metrics and emphasis without detail tables", () => {
    const { container } = render(
      <CodexPersonalCreditsPanel data={data} cycle={cycle} />,
    );
    const grid = screen.getByTestId("codex-capacity-metrics");
    expect(grid.children).toHaveLength(6);
    const expected = [
      ["完整周期 Token 等效容量", "5K"],
      ["已用额度 Token 等效容量", "1K"],
      ["剩余额度 Token 等效容量", "4K"],
      ["完整周期美元等效容量", "$250.00"],
      ["已用额度美元等效容量", "$50.00"],
      ["剩余额度美元等效容量", "$200.00"],
    ];
    expected.forEach(([label, value], index) => {
      expect(grid.children[index]).toHaveTextContent(label);
      expect(
        within(grid.children[index] as HTMLElement).getByText(value),
      ).toBeInTheDocument();
    });
    expect(metric("完整周期 Token 等效容量")).toHaveClass(
      "border-emerald-500/20",
    );
    expect(metric("完整周期美元等效容量")).toHaveClass("border-emerald-500/20");
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
    expect(container.querySelector("details")).toBeNull();
    expect(
      screen.queryByText(/模型与每日 Credits 明细/),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("gpt-6-astra")).not.toBeInTheDocument();
    expect(
      screen.getByRole("spinbutton", {
        name: "换算系数：每 1 USD 对应 Credits",
      }),
    ).toHaveAccessibleDescription("自定义等值，非官方账单汇率。");
  });
  it("renders raw values without a price table and allows custom USD conversion", () => {
    render(<CodexPersonalCreditsPanel data={data} cycle={cycle} />);
    expect(rawCredits()).toHaveTextContent("1,250");
    expect(
      within(metric("已用额度 Token 等效容量")).getByText("1K"),
    ).toBeInTheDocument();
    expect(
      within(metric("已用额度美元等效容量")).getByText("$50.00"),
    ).toBeInTheDocument();
    const input = screen.getByRole("spinbutton");
    fireEvent.change(input, { target: { value: "50" } });
    fireEvent.blur(input);
    expect(
      within(metric("已用额度美元等效容量")).getByText("$25.00"),
    ).toBeInTheDocument();
    expect(localStorage.getItem(CREDITS_PER_USD_STORAGE_KEY)).toBe("50");
    expect(rawCredits()).toHaveTextContent("1,250");
    expect(
      within(metric("已用额度 Token 等效容量")).getByText("1K"),
    ).toBeInTheDocument();
  });
  it("invalid conversion cannot turn into zero dollars", () => {
    render(<CodexPersonalCreditsPanel data={data} cycle={cycle} />);
    fireEvent.change(screen.getByRole("spinbutton"), {
      target: { value: "0" },
    });
    expect(screen.getByRole("alert")).toBeInTheDocument();
    expect(
      within(metric("已用额度美元等效容量")).getByText("待同步 / 待估算"),
    ).toBeInTheDocument();
    expect(rawCredits()).toHaveTextContent("1,250");
  });
  it("preserves totals when model endpoint fails", () => {
    render(
      <CodexPersonalCreditsPanel
        data={{ ...data, breakdownStatus: "unavailable" }}
        cycle={cycle}
      />,
    );
    expect(
      screen.queryByText(/模型明细缺失或校验失败/),
    ).not.toBeInTheDocument();
    expect(
      within(metric("完整周期美元等效容量")).getByText("$250.00"),
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
    expect(rawCredits()).toHaveTextContent("待同步 / 待估算");
    expect(
      within(metric("已用额度美元等效容量")).getByText("待同步 / 待估算"),
    ).toBeInTheDocument();
  });
  it("keeps warnings that affect totals while hiding model-only details", () => {
    render(
      <CodexPersonalCreditsPanel
        data={{ ...data, days: [{ ...data.days[0], credits: null }] }}
        cycle={{ ...cycle, startMs: Date.UTC(2026, 8, 10, 12) }}
      />,
    );
    expect(screen.getByText(/仅合计已知部分/)).toHaveTextContent("2026-09-10");
    expect(screen.getByText(/周期边界日计入整日数据/)).toHaveTextContent(
      "2026-09-10",
    );
    expect(
      within(metric("完整周期美元等效容量")).getByText("待同步 / 待估算"),
    ).toBeInTheDocument();
    expect(
      within(metric("完整周期 Token 等效容量")).getByText("5K"),
    ).toBeInTheDocument();
  });
});
