// Card ④ Queue: several games converted one after another on this computer; each finished
// package is handed to the console queue when it should install.

import { ArrowDown, ArrowUp, ListPlus, X } from "lucide-react";

import { Button, Card, Toggle } from "../../components";
import type { ConvertItem, ConvertThen } from "../../state/convertQueue";
import { useTr } from "../../state/lang";

export interface QueueCardProps {
  items: ConvertItem[];
  running: boolean;
  then: ConvertThen;
  onThen: (t: ConvertThen) => void;
  deleteAfter: boolean;
  onDeleteAfter: (on: boolean) => void;
  /** A console answers (installing is possible). */
  canInstall: boolean;
  /** The current source can be queued. */
  canAddCurrent: boolean;
  onAddCurrent: () => void;
  onScanFolder: () => void;
  /** Tick several images or archives in one picker. */
  onPickSeveral?: () => void;
  onStart: () => void;
  onStop: () => void;
  onRemove: (id: string) => void;
  onMove: (id: string, delta: -1 | 1) => void;
  onClearFinished: () => void;
}

function nameOf(source: string): string {
  return decodeURIComponent(source.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? source);
}

export function QueueCard(p: QueueCardProps) {
  const tr = useTr();
  const statusText: Record<ConvertItem["status"], string> = {
    pending: tr("cq_pending", undefined, "Waiting"),
    running: tr("cq_running", undefined, "Converting"),
    installing: tr("cq_installing", undefined, "Installing on the PS5"),
    handed: tr("cq_handed", undefined, "Sent to the console's queue"),
    done: tr("cq_done", undefined, "Done"),
    failed: tr("cq_failed", undefined, "Failed"),
  };
  const pending = p.items.filter((i) => i.status === "pending").length;
  const finished = p.items.some((i) => i.status === "done" || i.status === "failed" || i.status === "handed");
  return (
    <Card>
      <div className="flex flex-col gap-3">
        <div className="flex items-center justify-between gap-2">
          <div className="text-sm font-medium">{tr("cq_title", undefined, "④ Queue")}</div>
          <div className="flex gap-2">
            {p.running ? (
              <Button size="sm" onClick={p.onStop}>
                {tr("cq_stop", undefined, "Stop after this one")}
              </Button>
            ) : pending > 0 ? (
              <Button size="sm" variant="primary" onClick={p.onStart}>
                {tr("cq_start", { count: pending }, "Convert {count} queued")}
              </Button>
            ) : null}
            {finished && (
              <Button size="sm" variant="ghost" onClick={p.onClearFinished}>
                {tr("cq_clear", undefined, "Clear finished")}
              </Button>
            )}
          </div>
        </div>

        <div className="flex flex-wrap items-center gap-3 text-sm">
          <label className="flex items-center gap-2">
            {tr("cq_after", undefined, "After converting")}
            <select
              value={p.then}
              onChange={(e) => p.onThen(e.target.value as ConvertThen)}
              className="rounded-md border border-[var(--color-border)] bg-[var(--color-surface)] px-2 py-1 text-sm"
            >
              <option value="keep">{tr("cq_then_keep", undefined, "Keep the package")}</option>
              <option value="stream" disabled={!p.canInstall}>
                {tr("cq_then_stream", undefined, "Stream & install")}
              </option>
              <option value="upload" disabled={!p.canInstall}>
                {tr("cq_then_upload", undefined, "Upload & install")}
              </option>
            </select>
          </label>
          {p.then !== "keep" && (
            <Toggle
              checked={p.deleteAfter}
              onChange={p.onDeleteAfter}
              label={tr("cq_delete_after", undefined, "Delete the package once installed")}
            />
          )}
        </div>

        <div className="flex flex-wrap gap-2">
          <Button size="sm" disabled={!p.canAddCurrent} onClick={p.onAddCurrent}>
            {tr("cq_add_current", undefined, "Add this game to the queue")}
          </Button>
          <Button size="sm" variant="ghost" leftIcon={<ListPlus size={14} />} onClick={p.onScanFolder}>
            {tr("batch_scan", undefined, "Add games from a folder…")}
          </Button>
          {p.onPickSeveral && (
            <Button size="sm" variant="ghost" leftIcon={<ListPlus size={14} />} onClick={p.onPickSeveral}>
              {tr("cq_pick_several", undefined, "Pick several…")}
            </Button>
          )}
        </div>

        {p.items.length > 0 && (
          <ul className="divide-y divide-[var(--color-border)] rounded-md border border-[var(--color-border)]">
            {p.items.map((i, idx) => (
              <li key={i.id} className="flex items-center gap-2 px-3 py-2 text-sm">
                <div className="min-w-0 flex-1">
                  <div className="truncate font-medium" title={i.source}>
                    {nameOf(i.source)}
                  </div>
                  <div
                    className={`truncate text-xs ${i.status === "failed" ? "text-[var(--color-bad)]" : "text-[var(--color-muted)]"}`}
                  >
                    {statusText[i.status]}
                    {i.error ? ` — ${i.error}` : i.packagePath ? ` — ${i.packagePath}` : ""}
                  </div>
                </div>
                {i.status === "pending" && (
                  <>
                    <button
                      type="button"
                      disabled={idx === 0}
                      onClick={() => p.onMove(i.id, -1)}
                      aria-label={tr("queue_move_up", undefined, "Move up")}
                      className="rounded p-1 text-[var(--color-muted)] hover:bg-[var(--color-surface-3)] disabled:opacity-30"
                    >
                      <ArrowUp size={14} />
                    </button>
                    <button
                      type="button"
                      disabled={idx === p.items.length - 1}
                      onClick={() => p.onMove(i.id, 1)}
                      aria-label={tr("queue_move_down", undefined, "Move down")}
                      className="rounded p-1 text-[var(--color-muted)] hover:bg-[var(--color-surface-3)] disabled:opacity-30"
                    >
                      <ArrowDown size={14} />
                    </button>
                  </>
                )}
                {i.status !== "running" && i.status !== "installing" && (
                  <button
                    type="button"
                    onClick={() => p.onRemove(i.id)}
                    aria-label={tr("queue_remove", undefined, "Remove from queue")}
                    className="rounded p-1 text-[var(--color-muted)] hover:bg-[var(--color-surface-3)]"
                  >
                    <X size={14} />
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
    </Card>
  );
}
