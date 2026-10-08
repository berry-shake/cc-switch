import { describe, expect, it } from "vitest";
import { withOmpAliases } from "@/i18n";

describe("OMP translation aliases", () => {
  it("rewrites OMP path hints without changing the original Pi hints", () => {
    const pi = { restored: "Pi: ~/.pi/agent/APPEND_SYSTEM.md" };
    expect(withOmpAliases({ pi })).toEqual({
      pi,
      omp: { restored: "OMP: ~/.omp/agent/APPEND_SYSTEM.md" },
    });
  });

  it("handles directory placeholders, including Windows paths", () => {
    expect(
      withOmpAliases({
        browsePlaceholderPi: "/home/<user>/.pi/agent",
        piWindowsPath: "C:\\Users\\user\\.pi\\agent",
      }),
    ).toMatchObject({
      browsePlaceholderOmp: "/home/<user>/.omp/agent",
      ompWindowsPath: "C:\\Users\\user\\.omp\\agent",
    });
  });

  it("keeps explicit OMP translations and unrelated path segments", () => {
    expect(
      withOmpAliases({ pi: "/tmp/.pipeline/Pi", omp: "explicit" }),
    ).toEqual({
      pi: "/tmp/.pipeline/Pi",
      omp: "explicit",
    });
  });
});
