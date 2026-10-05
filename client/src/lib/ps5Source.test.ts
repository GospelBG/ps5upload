import { describe, expect, it } from "vitest";

import { ps5SourceProblem, sourceCandidates } from "./ps5Source";

const roster = [
  { id: "a", name: "Pro", host: "10.0.0.2" },
  { id: "b", name: "Phat", host: "10.0.0.3:9113" },
  { id: "c", name: "Blank", host: "" },
];

describe("PS5 to PS5 source", () => {
  it("offers every other console in the roster, never the destination itself", () => {
    expect(sourceCandidates(roster, "10.0.0.2:9120").map((p) => p.id)).toEqual(["b"]);
    expect(sourceCandidates(roster, "10.0.0.3").map((p) => p.id)).toEqual(["a"]);
  });

  it("names what is missing before it lets the copy start", () => {
    const ok = {
      fromHost: "10.0.0.3",
      srcPath: "/data/games/X",
      toHost: "10.0.0.2",
      destPath: "/data/games",
    };
    expect(ps5SourceProblem(ok)).toBeNull();
    expect(ps5SourceProblem({ ...ok, toHost: "" })).toBe("no_destination_console");
    expect(ps5SourceProblem({ ...ok, fromHost: "" })).toBe("no_source_console");
    expect(ps5SourceProblem({ ...ok, fromHost: "10.0.0.2:9114" })).toBe("same_console");
    expect(ps5SourceProblem({ ...ok, srcPath: "data/x" })).toBe("no_source_path");
    expect(ps5SourceProblem({ ...ok, destPath: "" })).toBe("no_destination_path");
  });
});
