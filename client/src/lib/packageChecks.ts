// Checks the package viewer shows against the selected console: can it run
// this package, is it already installed, and does it fit. Pure, so the panel
// stays a view and these stay tested.

export type Verdict = "ok" | "warn" | "bad" | "unknown";

/** "01.002.000" / "1.2" → [1, 2, 0] for comparison. */
function parts(v: string): number[] {
  return v.split(".").map((p) => parseInt(p, 10) || 0);
}

/** Numeric, part-by-part; missing parts count as 0 ("01.002" = "01.002.000"). */
export function versionCompare(a: string, b: string): number {
  const x = parts(a);
  const y = parts(b);
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const d = (x[i] ?? 0) - (y[i] ?? 0);
    if (d !== 0) return d;
  }
  return 0;
}

/** Whether the console's firmware meets the package's minimum. */
export function firmwareCheck(minFw: string | null, consoleFw: string | null) {
  const verdict: Verdict =
    !minFw || !consoleFw
      ? "unknown"
      : versionCompare(consoleFw, minFw) >= 0
        ? "ok"
        : "bad";
  return { verdict, minFw, consoleFw };
}

/** This package against the version installed on the console. */
export function installedCheck(pkgVer: string, installedVer: string | null) {
  if (!installedVer) {
    return { verdict: "ok" as Verdict, relation: "not-installed" as const };
  }
  const c = versionCompare(pkgVer, installedVer);
  if (c > 0) return { verdict: "ok" as Verdict, relation: "newer" as const };
  if (c < 0) return { verdict: "warn" as Verdict, relation: "older" as const };
  return { verdict: "warn" as Verdict, relation: "same" as const };
}

/** Whether `size` fits in `freeBytes`; unknown when free space can't be read. */
export function spaceCheck(size: number, freeBytes: number | null) {
  if (freeBytes == null) return { verdict: "unknown" as Verdict, shortBy: 0 };
  return size <= freeBytes
    ? { verdict: "ok" as Verdict, shortBy: 0 }
    : { verdict: "bad" as Verdict, shortBy: size - freeBytes };
}
