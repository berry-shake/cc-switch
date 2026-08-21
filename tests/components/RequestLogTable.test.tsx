import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { RequestLogTable } from "@/components/usage/RequestLogTable";
import type { RequestLog, UsageRangeSelection } from "@/types/usage";

const useRequestLogsMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (
      key: string,
      options?: {
        defaultValue?: string;
      },
    ) => options?.defaultValue ?? key,
    i18n: {
      resolvedLanguage: "en",
      language: "en",
    },
  }),
}));

vi.mock("@/lib/query/usage", () => ({
  useRequestLogs: (args: unknown) => useRequestLogsMock(args),
}));

vi.mock("@/components/ui/button", () => ({
  Button: ({ children, ...props }: any) => (
    <button {...props}>{children}</button>
  ),
}));

vi.mock("@/components/ui/input", () => ({
  Input: (props: any) => <input {...props} />,
}));

vi.mock("@/components/ui/select", () => ({
  Select: ({ children }: any) => <div>{children}</div>,
  SelectTrigger: ({ children, ...props }: any) => (
    <button type="button" {...props}>
      {children}
    </button>
  ),
  SelectValue: ({ placeholder }: any) => <span>{placeholder ?? null}</span>,
  SelectContent: () => null,
  SelectItem: () => null,
}));

vi.mock("@/components/ui/table", () => ({
  Table: ({ children }: any) => <table>{children}</table>,
  TableBody: ({ children }: any) => <tbody>{children}</tbody>,
  TableCell: ({ children, ...props }: any) => <td {...props}>{children}</td>,
  TableHead: ({ children, ...props }: any) => <th {...props}>{children}</th>,
  TableHeader: ({ children }: any) => <thead>{children}</thead>,
  TableRow: ({ children }: any) => <tr>{children}</tr>,
}));

describe("RequestLogTable", () => {
  beforeEach(() => {
    useRequestLogsMock.mockReset();
    useRequestLogsMock.mockImplementation(
      ({ page = 0, pageSize = 20 }: { page?: number; pageSize?: number }) => ({
        data: {
          data: [],
          total: 120,
          page,
          pageSize,
        },
        isLoading: false,
      }),
    );
  });

  it("keeps the empty state aligned with all request-log columns", () => {
    render(
      <RequestLogTable
        range={{ preset: "today" }}
        rangeLabel="Today"
        appType="all"
        refreshIntervalMs={0}
      />,
    );

    expect(screen.getAllByRole("columnheader")).toHaveLength(11);
    expect(screen.getByText("usage.noData").closest("td")).toHaveAttribute(
      "colspan",
      "11",
    );
  });

  it("resets pagination when the dashboard range changes", async () => {
    const initialRange: UsageRangeSelection = { preset: "today" };
    const nextRange: UsageRangeSelection = {
      preset: "custom",
      customStartDate: 1_710_000_000,
      customEndDate: 1_710_086_400,
    };

    const { rerender } = render(
      <RequestLogTable
        range={initialRange}
        rangeLabel="Today"
        appType="all"
        refreshIntervalMs={0}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "2" }));

    await waitFor(() => {
      expect(useRequestLogsMock).toHaveBeenLastCalledWith(
        expect.objectContaining({
          page: 1,
          range: initialRange,
        }),
      );
    });

    rerender(
      <RequestLogTable
        range={nextRange}
        rangeLabel="Custom"
        appType="all"
        refreshIntervalMs={0}
      />,
    );

    await waitFor(() => {
      expect(useRequestLogsMock).toHaveBeenLastCalledWith(
        expect.objectContaining({
          page: 0,
          range: nextRange,
        }),
      );
    });
  });

  it("resets pagination when the dashboard app filter changes", async () => {
    const range: UsageRangeSelection = { preset: "today" };
    const { rerender } = render(
      <RequestLogTable
        range={range}
        rangeLabel="Today"
        appType="all"
        refreshIntervalMs={0}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "2" }));

    await waitFor(() => {
      expect(useRequestLogsMock).toHaveBeenLastCalledWith(
        expect.objectContaining({
          page: 1,
          range,
        }),
      );
    });

    rerender(
      <RequestLogTable
        range={range}
        rangeLabel="Today"
        appType="claude"
        refreshIntervalMs={0}
      />,
    );

    await waitFor(() => {
      expect(useRequestLogsMock).toHaveBeenLastCalledWith(
        expect.objectContaining({
          page: 0,
          range,
        }),
      );
    });
  });

  it("renders cache read and cache creation as independent columns", () => {
    const log = {
      requestId: "codex-session-row",
      providerId: "codex-session",
      providerName: "Codex (Session)",
      appType: "codex",
      model: "gpt-5.6-sol",
      costMultiplier: "2.5",
      inputTokens: 186_301,
      outputTokens: 3_980,
      cacheReadTokens: 185_088,
      cacheCreationTokens: 0,
      inputTokenSemantics: 1,
      inputCostUsd: "0.006065",
      outputCostUsd: "0.1194",
      cacheReadCostUsd: "0.092544",
      cacheCreationCostUsd: "0",
      totalCostUsd: "0.5450225",
      isStreaming: false,
      latencyMs: 0,
      statusCode: 200,
      createdAt: 1_787_329_652,
      dataSource: "codex_session",
    } satisfies RequestLog;

    const legacyLog = {
      ...log,
      requestId: "legacy-codex-session-row",
      inputTokenSemantics: 0,
    } satisfies RequestLog;

    useRequestLogsMock.mockReturnValue({
      data: {
        data: [log, legacyLog],
        total: 2,
        page: 0,
        pageSize: 20,
      },
      isLoading: false,
    });

    render(
      <RequestLogTable
        range={{ preset: "today" }}
        rangeLabel="Today"
        appType="codex"
        refreshIntervalMs={0}
      />,
    );

    const headers = screen.getAllByRole("columnheader");
    expect(headers).toHaveLength(11);
    expect(headers[3]).toHaveTextContent("usage.freshInput");
    expect(headers[4]).toHaveTextContent("usage.cacheReadTokens");
    expect(headers[5]).toHaveTextContent("usage.cacheCreationTokens");
    expect(headers[6]).toHaveTextContent("usage.outputTokens");

    const dataRows = screen.getAllByRole("row").slice(1);
    const cells = within(dataRows[0]).getAllByRole("cell");
    expect(cells).toHaveLength(11);
    expect(cells[3]).toHaveTextContent("1,213");
    expect(cells[4]).toHaveTextContent("185,088");
    expect(cells[5]).toHaveTextContent("0");
    expect(cells[6]).toHaveTextContent("3,980");

    const legacyCells = within(dataRows[1]).getAllByRole("cell");
    expect(legacyCells[5]).toHaveTextContent("—");
    expect(legacyCells[5].querySelector("span")).toHaveAttribute(
      "title",
      "common.unknown",
    );
  });
});
