import { describe, expect, it } from "vitest";

import type { CheatRepoEntry, CheatTitle, InstalledTitle } from "../api/ps5";
import {
  buildCheatGames,
  cheatSections,
  compareVersions,
  filterCheatGames,
  hasVersionMatch,
  rankCheatFiles,
  sameVersion,
} from "./cheatGames";

const inst = (titleId: string, titleName: string, system = false) =>
  ({ titleId, titleName, system, origin: "registered", imageBacked: false, source: "" }) as InstalledTitle;
const entry = (filename: string, game_version: string, format = "mc4"): CheatRepoEntry => ({
  filename,
  game_title: "",
  format,
  repo_id: "henmix",
  title_id: filename.slice(0, 9),
  game_version,
});
const dl = (title_id: string, version = "", running = false): CheatTitle => ({
  title_id,
  name: title_id,
  version,
  running,
});

describe("versions", () => {
  it("compares by number, not text", () => {
    expect(sameVersion("01.000.016", "1.0.16")).toBe(true);
    expect(sameVersion("01.000.016", "01.000.021")).toBe(false);
    expect(compareVersions("01.10", "01.05")).toBeGreaterThan(0);
    expect(sameVersion("", "01.00")).toBe(false);
    expect(sameVersion(null, null)).toBe(false);
  });
});

describe("buildCheatGames", () => {
  const index = [
    entry("PPSA23226_01.000.010.json", "01.000.010", "json"),
    entry("PPSA23226_01.000.016_a.mc4", "01.000.016"),
    entry("PPSA23226_01.000.016_x.cht", "01.000.016", "cht"),
    entry("PPSA19534_01.000.016_b.mc4", "01.000.016"),
  ];

  it("lists installed games with what the collection has for each", () => {
    const games = buildCheatGames({
      installed: [inst("PPSA23226", "Black Myth: Wukong"), inst("PPSA01650", "YouTube"), inst("NPXS40000", "Sys", true)],
      downloaded: [],
      index,
    });
    expect(games.map((g) => g.titleId).sort()).toEqual(["PPSA01650", "PPSA23226"]);
    const wukong = games.find((g) => g.titleId === "PPSA23226")!;
    // an unreadable format is not offered
    expect(wukong.available.map((e) => e.format)).toEqual(["json", "mc4"]);
  });

  it("keeps a downloaded cheat for a game that is not installed", () => {
    const games = buildCheatGames({
      installed: [],
      downloaded: [dl("CUSA09193", "01.05")],
      index: [],
      repoNames: new Map([["CUSA09193", "RESIDENT EVIL 3"]]),
    });
    expect(games[0]).toMatchObject({ name: "RESIDENT EVIL 3", installed: false, downloaded: true, downloadedVersion: "01.05" });
  });

  it("groups: playing, ready, available, none", () => {
    const games = buildCheatGames({
      installed: [
        inst("PPSA23226", "Black Myth"),
        inst("PPSA19534", "Battlefield 6"),
        inst("PPSA01650", "YouTube"),
        inst("PPSA26344", "Ghost"),
      ],
      downloaded: [dl("PPSA19534", "01.000.016")],
      index,
      runningTitleId: "ppsa26344",
    });
    const sections = cheatSections(games);
    expect(sections.map((s) => [s.key, s.games.map((g) => g.titleId)])).toEqual([
      ["playing", ["PPSA26344"]],
      ["ready", ["PPSA19534"]],
      ["available", ["PPSA23226"]],
      ["none", ["PPSA01650"]],
    ]);
  });

  it("searches by name or id", () => {
    const games = buildCheatGames({ installed: [inst("PPSA23226", "Black Myth"), inst("PPSA19534", "Battlefield")], downloaded: [], index });
    expect(filterCheatGames(games, "myth").map((g) => g.titleId)).toEqual(["PPSA23226"]);
    expect(filterCheatGames(games, "19534").map((g) => g.titleId)).toEqual(["PPSA19534"]);
  });
});

describe("rankCheatFiles", () => {
  const files = [
    entry("PPSA23226_01.000.010.json", "01.000.010", "json"),
    entry("PPSA23226_01.000.021_c.mc4", "01.000.021"),
    entry("PPSA23226_01.000.016_a.mc4", "01.000.016"),
  ];
  it("puts the installed version first, then newest", () => {
    expect(rankCheatFiles(files, "01.000.016").map((e) => e.game_version)).toEqual([
      "01.000.016",
      "01.000.021",
      "01.000.010",
    ]);
    expect(hasVersionMatch(files, "01.000.016")).toBe(true);
    expect(hasVersionMatch(files, "01.000.099")).toBe(false);
  });
  it("is newest first when the installed version is unknown", () => {
    expect(rankCheatFiles(files, null)[0].game_version).toBe("01.000.021");
  });
});
