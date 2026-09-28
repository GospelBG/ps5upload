// "Add games from a folder…": the picked folder's immediate children as batch entries. One level
// only; games, packages, archives and images start checked, anything else unchecked.

import { localFs } from "../api/localFs";
import { remoteApi } from "../api/remote";
import type { BatchEntry } from "../state/uploadBatch";
import { classifyScanEntry } from "./uploadBatch";
import { parseRemotePath, remotePath } from "./remotePath";

export async function scanChildren(folder: string): Promise<BatchEntry[]> {
  const entry = (path: string, name: string, isDir: boolean, size: number): BatchEntry => ({
    path,
    isDir,
    // A folder's listed size is not its contents'.
    size: isDir ? null : size,
    include: classifyScanEntry(name, isDir) !== "other",
  });
  const parsed = parseRemotePath(folder);
  if (parsed) {
    const out: BatchEntry[] = [];
    let cursor: string | undefined;
    for (;;) {
      const page = await remoteApi.listDir(folder, cursor);
      for (const e of page.entries) {
        const child = remotePath(parsed.connectionId, `${parsed.path.replace(/\/+$/, "")}/${e.name}`);
        out.push(entry(child, e.name, e.is_dir, e.size));
      }
      if (!page.next_cursor) return out;
      cursor = page.next_cursor;
    }
  }
  return (await localFs.listDir(folder)).map((e) => entry(e.path, e.name, e.is_dir, e.size));
}
