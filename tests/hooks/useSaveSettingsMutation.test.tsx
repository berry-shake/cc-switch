import type { PropsWithChildren } from "react";
import { act, renderHook } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { useSaveSettingsMutation } from "@/lib/query/mutations";
import { settingsApi } from "@/lib/api";
import type { Settings } from "@/types";

describe("settings session cache invalidation", () => {
  it.each([true, false])(
    "invalidates old scans only when Claude roots change: %s",
    async (changed) => {
      const queryClient = new QueryClient({
        defaultOptions: {
          queries: { retry: false },
          mutations: { retry: false },
        },
      });
      const settings = {
        claudeConfigDir: "/primary",
        claudeAdditionalConfigDirs: ["/a"],
      } as Settings;
      queryClient.setQueryData(["settings"], settings);
      queryClient.setQueryData(["sessions"], []);
      for (const key of ["sessionTranscript", "sessionBlockContent"]) {
        queryClient.setQueryData(
          [key, "claude", "/a/file.jsonl"],
          ["old-root"],
        );
        queryClient.setQueryData([key, "codex", "/codex/file.jsonl"], ["keep"]);
      }
      queryClient.setQueryData(
        ["sessionMessages", "claude", "/a/file.jsonl"],
        [],
      );
      queryClient.setQueryData(
        ["sessionMessages", "codex", "/codex/file.jsonl"],
        [],
      );
      const save = vi.spyOn(settingsApi, "save").mockResolvedValue(true);
      const cancel = vi.spyOn(queryClient, "cancelQueries");
      const wrapper = ({ children }: PropsWithChildren) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );
      const { result, unmount } = renderHook(() => useSaveSettingsMutation(), {
        wrapper,
      });
      await act(async () => {
        await result.current.mutateAsync({
          ...settings,
          claudeAdditionalConfigDirs: changed ? ["/b"] : ["/a"],
        });
      });
      expect(queryClient.getQueryState(["sessions"])?.isInvalidated).toBe(
        changed,
      );
      for (const key of ["sessionTranscript", "sessionBlockContent"]) {
        expect(
          queryClient.getQueryData([key, "claude", "/a/file.jsonl"]),
        ).toEqual(changed ? undefined : ["old-root"]);
        expect(
          queryClient.getQueryData([key, "codex", "/codex/file.jsonl"]),
        ).toEqual(["keep"]);
      }
      expect(
        queryClient.getQueryData([
          "sessionMessages",
          "codex",
          "/codex/file.jsonl",
        ]),
      ).toEqual([]);
      if (changed) {
        expect(cancel).toHaveBeenCalledWith({ queryKey: ["sessions"] });
        expect(
          queryClient.getQueryState([
            "sessionMessages",
            "claude",
            "/a/file.jsonl",
          ]),
        ).toBeUndefined();
      } else expect(cancel).not.toHaveBeenCalled();
      unmount();
      queryClient.clear();
      save.mockRestore();
    },
  );
});
