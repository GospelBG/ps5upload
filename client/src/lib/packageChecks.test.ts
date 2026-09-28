import { describe, expect, it } from "vitest";
import { firmwareCheck, installedCheck, spaceCheck, versionCompare } from "./packageChecks";

describe("package checks", () => {
  it("firmware: bad when the console is older than the package needs", () => {
    expect(firmwareCheck("12.00", "5.10").verdict).toBe("bad");
    expect(firmwareCheck("5.10", "9.60").verdict).toBe("ok");
    expect(firmwareCheck("5.10", "5.10").verdict).toBe("ok");
    expect(firmwareCheck(null, "9.60").verdict).toBe("unknown");
    expect(firmwareCheck("5.10", null).verdict).toBe("unknown");
  });
  it("versions compare numerically across 2- and 3-part forms", () => {
    expect(versionCompare("01.04", "01.02")).toBeGreaterThan(0);
    expect(versionCompare("01.002.000", "01.002")).toBe(0);
    expect(versionCompare("01.10", "01.9")).toBeGreaterThan(0);
  });
  it("installed: newer / older / same / not installed", () => {
    expect(installedCheck("01.04", null).relation).toBe("not-installed");
    expect(installedCheck("01.04", "01.02").relation).toBe("newer");
    expect(installedCheck("01.02", "01.04").relation).toBe("older");
    expect(installedCheck("01.04", "01.04").relation).toBe("same");
  });
  it("space: bad with the shortfall when it clearly doesn't fit; unknown without a reading", () => {
    expect(spaceCheck(100, 40)).toEqual({ verdict: "bad", shortBy: 60 });
    expect(spaceCheck(100, 400).verdict).toBe("ok");
    expect(spaceCheck(100, null).verdict).toBe("unknown");
  });
});
