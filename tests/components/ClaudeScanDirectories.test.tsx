import { useState } from "react";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ClaudeScanDirectories } from "@/components/settings/ClaudeScanDirectories";
import { settingsApi } from "@/lib/api/settings";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, args?: { path?: string }) =>
      args?.path ? `${key}:${args.path}` : key,
  }),
}));
vi.mock("@/lib/api/settings", () => ({
  settingsApi: {
    inspectClaudeScanDirectories: vi.fn(),
    pickDirectory: vi.fn(),
  },
}));

function Form({ initial = [] }: { initial?: string[] }) {
  const [dirs, setDirs] = useState(initial);
  return (
    <ClaudeScanDirectories primary="/primary" dirs={dirs} onChange={setDirs} />
  );
}

describe("ClaudeScanDirectories", () => {
  beforeEach(() => {
    vi.resetAllMocks();
    vi.mocked(settingsApi.inspectClaudeScanDirectories).mockImplementation(
      async (dirs) => dirs.map((path) => ({ path, status: "ready" })),
    );
  });

  it("adds individual roots, shows their status, and removes only the selected row", async () => {
    render(<Form initial={["/account-a"]} />);
    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: " /account-b " },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "settings.claudeScan.add" }),
    );
    await screen.findByText("/account-b");
    await waitFor(() =>
      expect(
        screen.getAllByText("settings.claudeScan.status.ready"),
      ).toHaveLength(2),
    );
    expect(settingsApi.inspectClaudeScanDirectories).toHaveBeenCalledWith(
      ["/account-a", "/account-b"],
      "/primary",
    );
    fireEvent.click(
      screen.getByRole("button", {
        name: "settings.claudeScan.remove:/account-a",
      }),
    );
    expect(screen.queryByText("/account-a")).not.toBeInTheDocument();
    expect(screen.getByText("/account-b")).toBeInTheDocument();
  });

  it.each(["duplicate", "invalid"] as const)(
    "rejects a %s root without changing the list",
    async (status) => {
      vi.mocked(settingsApi.inspectClaudeScanDirectories).mockResolvedValue([
        { path: "/alias", status },
      ]);
      render(<Form />);
      fireEvent.change(screen.getByRole("textbox"), {
        target: { value: "/alias" },
      });
      fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
      expect(await screen.findByRole("alert")).toHaveTextContent(
        `settings.claudeScan.status.${status}`,
      );
      expect(screen.queryByText("/alias")).not.toBeInTheDocument();
    },
  );

  it("allows temporarily missing directories and supports browse/cancel", async () => {
    vi.mocked(settingsApi.pickDirectory)
      .mockResolvedValueOnce(null)
      .mockResolvedValueOnce("/external/account");
    vi.mocked(settingsApi.inspectClaudeScanDirectories).mockResolvedValue([
      { path: "/external/account", status: "missing" },
    ]);
    render(<Form />);
    const browse = screen.getByRole("button", {
      name: "settings.browseDirectory",
    });
    fireEvent.click(browse);
    await waitFor(() => expect(browse).not.toBeDisabled());
    expect(settingsApi.inspectClaudeScanDirectories).not.toHaveBeenCalled();
    fireEvent.click(browse);
    expect(await screen.findByText("/external/account")).toBeInTheDocument();
    expect(
      await screen.findByText("settings.claudeScan.status.missing"),
    ).toBeInTheDocument();
  });

  it("ignores an inspection response for a root removed while checking", async () => {
    let resolve!: (value: { path: string; status: "ready" }[]) => void;
    vi.mocked(settingsApi.inspectClaudeScanDirectories).mockReturnValueOnce(
      new Promise((done) => {
        resolve = done;
      }),
    );
    render(<Form initial={["/slow"]} />);
    await waitFor(() =>
      expect(settingsApi.inspectClaudeScanDirectories).toHaveBeenCalled(),
    );
    fireEvent.click(
      screen.getByRole("button", { name: "settings.claudeScan.remove:/slow" }),
    );
    resolve([{ path: "/slow", status: "ready" }]);
    await waitFor(() =>
      expect(
        screen.queryByText("settings.claudeScan.status.ready"),
      ).not.toBeInTheDocument(),
    );
  });
});
