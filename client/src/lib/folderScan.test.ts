import { describe, expect, it, vi } from "vitest";

const listDir = vi.fn();
const remoteList = vi.fn();
vi.mock("../api/localFs", () => ({ localFs: { listDir: (p: string) => listDir(p) } }));
vi.mock("../api/remote", () => ({ remoteApi: { listDir: (p: string, c?: string) => remoteList(p, c) } }));

import { scanChildren } from "./folderScan";

describe("Add games from a folder", () => {
  it("lists the folder's children, checking the playable ones", async () => {
    listDir.mockResolvedValue([
      { name: "Game A", path: "/g/Game A", is_dir: true, size: 0 },
      { name: "b.pkg", path: "/g/b.pkg", is_dir: false, size: 10 },
      { name: "notes.txt", path: "/g/notes.txt", is_dir: false, size: 1 },
    ]);
    expect(await scanChildren("/g")).toEqual([
      { path: "/g/Game A", isDir: true, size: null, include: true },
      { path: "/g/b.pkg", isDir: false, size: 10, include: true },
      { path: "/g/notes.txt", isDir: false, size: 1, include: false },
    ]);
  });

  it("reads a saved server's folder page by page", async () => {
    remoteList
      .mockResolvedValueOnce({ entries: [{ name: "x.7z", is_dir: false, size: 5 }], next_cursor: "c" })
      .mockResolvedValueOnce({ entries: [{ name: "G", is_dir: true, size: 0 }], next_cursor: null });
    const got = await scanChildren("remote://nas/dl");
    expect(got.map((e) => e.path)).toEqual(["remote://nas/dl/x.7z", "remote://nas/dl/G"]);
    expect(remoteList).toHaveBeenLastCalledWith("remote://nas/dl", "c");
  });
});
