import { describe, it, expect } from "vitest";
import { parsePS5Firmware, firmwareMajor, fpkgUnsupportedFirmware } from "./ps5Firmware";

describe("parsePS5Firmware", () => {
  it("extracts from 'releases/09.60' kernel string", () => {
    expect(
      parsePS5Firmware(
        "FreeBSD 11.0-RELEASE-p0 #1 r218215/releases/09.60 Jul 18 2023"
      )
    ).toBe("9.60");
  });

  it("extracts from 'releases/10.00'", () => {
    expect(
      parsePS5Firmware("FreeBSD 11.0 r222222/releases/10.00-DEBUG")
    ).toBe("10.00");
  });

  it("strips leading zero from major", () => {
    expect(parsePS5Firmware("r/releases/05.00 foo")).toBe("5.00");
  });

  it("returns null for null/empty/unknown strings", () => {
    expect(parsePS5Firmware(null)).toBeNull();
    expect(parsePS5Firmware("")).toBeNull();
    expect(parsePS5Firmware("unknown")).toBeNull();
  });

  it("falls back to a bare NN.NN substring", () => {
    expect(parsePS5Firmware("kernel tag 9.00")).toBe("9.00");
  });

  it("prefers the releases/ match over a trailing date number", () => {
    // "releases/09.60" should win over the ".11" in "11.0"
    expect(
      parsePS5Firmware("FreeBSD 11.0 r218215/releases/09.60: Jul 18 2023")
    ).toBe("9.60");
  });
});

describe("firmwareMajor (Stream Install FW gate)", () => {
  it("returns the integer major below the FW-11 cutoff (stream BLOCKED)", () => {
    expect(
      firmwareMajor("FreeBSD 11.0 r218215/releases/09.60 Jul 18 2023")
    ).toBe(9);
    expect(firmwareMajor("r/releases/05.00")).toBe(5);
    expect(firmwareMajor("r/releases/10.40")).toBe(10);
    // Major extraction is descriptive only; Stream support is not gated by it.
    expect(firmwareMajor("r/releases/09.60")! < 11).toBe(true);
    expect(firmwareMajor("r/releases/10.40")! < 11).toBe(true);
  });

  it("returns >= 11 at and above the cutoff (stream ALLOWED with advisory)", () => {
    expect(firmwareMajor("r/releases/11.00")).toBe(11);
    expect(firmwareMajor("r/releases/12.40")).toBe(12);
    // Newer major versions parse the same way.
    expect(firmwareMajor("r/releases/12.40")! >= 11).toBe(true);
    expect(firmwareMajor("r/releases/09.60")! >= 11).toBe(false);
  });

  it("returns null when the firmware can't be parsed (gate does NOT block)", () => {
    expect(firmwareMajor(null)).toBeNull();
    expect(firmwareMajor("")).toBeNull();
    expect(firmwareMajor("unknown build")).toBeNull();
  });
});

describe("fpkgUnsupportedFirmware", () => {
  it("flags firmware newer than 11.60 (the real 13.60 kernel string)", () => {
    expect(fpkgUnsupportedFirmware("r229358/releases/13.60 Jul 17 2026 02:16:29")).toBe("13.60");
    expect(fpkgUnsupportedFirmware("r1/releases/11.61 x")).toBe("11.61");
    expect(fpkgUnsupportedFirmware("r1/releases/12.00 x")).toBe("12.00");
  });
  it("leaves 11.60 and older alone", () => {
    expect(fpkgUnsupportedFirmware("r1/releases/11.60 x")).toBeNull();
    expect(fpkgUnsupportedFirmware("r1/releases/9.60 x")).toBeNull();
    expect(fpkgUnsupportedFirmware("r1/releases/5.10 x")).toBeNull();
  });
  it("never warns on an unknown firmware", () => {
    expect(fpkgUnsupportedFirmware(null)).toBeNull();
    expect(fpkgUnsupportedFirmware("unknown build")).toBeNull();
  });
});
