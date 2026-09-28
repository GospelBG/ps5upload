import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { QueueRow, queueItemsForHost } from "./QueuePanel";
import en from "../../i18n/locales/en";
import type { QueueItem } from "../../state/uploadQueue";

const base = {
  strategy: "overwrite",
  reconcileMode: "fast",
  excludes: [],
  mountAfterUpload: false,
  mountReadOnly: true,
  registerAfterUpload: false,
  txIdHex: "00",
  bytesSent: 0,
  totalBytes: 0,
  bytesPerSec: 0,
  filesFinalized: 0,
  filesFinalizingTotal: 0,
  mountedAt: null,
  registeredAs: null,
  mountWarnings: [],
  error: null,
  errorReason: null,
  errorDetail: null,
  addedAt: 1,
  startedAt: null,
  completedAt: null,
  resolvedDest: "",
} as const;

const item = (over: Record<string, unknown>) => ({ ...base, ...over }) as unknown as QueueItem;

describe("Queue panel", () => {
  const failedStream = item({
    id: "a",
    sourceKind: "install",
    sourcePath: "pc:/g/a.pkg",
    displayName: "Game A",
    addr: "10.0.0.2:9113",
    install: { via: "stream", source: "/g/a.pkg" },
    status: "failed",
    error: "unreachable",
    fallbackToUpload: true,
  });
  const waitingLibrary = item({
    id: "c",
    sourceKind: "install",
    sourcePath: "ps5:/q.pkg",
    displayName: "Game C",
    addr: "10.0.0.2:9113",
    install: { via: "library", path: "/q.pkg" },
    status: "pending",
  });
  const otherConsole = item({
    id: "b",
    sourceKind: "install",
    sourcePath: "ps5:/p.pkg",
    displayName: "Game B",
    addr: "10.0.0.3:9113",
    install: { via: "library", path: "/p.pkg" },
    status: "pending",
  });
  const noop = () => {};
  const row = (it: QueueItem, position?: number) =>
    renderToStaticMarkup(
      <QueueRow
        item={it}
        position={position}
        onMoveUp={noop}
        onMoveDown={noop}
        onRemove={noop}
        onCancel={noop}
        onRetry={noop}
        onRetryViaUpload={noop}
      />,
    );

  it("shows only the given console's items", () => {
    const items = [failedStream, waitingLibrary, otherConsole];
    expect(queueItemsForHost(items, "10.0.0.2").map((i) => i.id)).toEqual(["a", "c"]);
    expect(queueItemsForHost(items, undefined)).toHaveLength(3);
  });

  it("offers Retry via upload on a failed stream install that asked for it", () => {
    const html = row(failedStream);
    expect(html).toContain("Retry via upload");
    expect(html).toContain("Retry");
  });

  it("says where an install comes from and where it waits in line", () => {
    expect(row(failedStream)).toContain("Stream install from this computer");
    const waiting = row(waitingLibrary, 1);
    expect(waiting).toContain("Install from the PS5");
    expect(waiting).toContain("Queued (#1)");
    expect(waiting).not.toContain("Overwrite");
  });

  it("titles the panel Queue", () => {
    expect(en.queue_title).toBe("Queue");
  });
});
