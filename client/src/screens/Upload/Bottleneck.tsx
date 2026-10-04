import { Loader2 } from "lucide-react";

import { useTr } from "../../state/lang";
import { formatBytes } from "../../lib/format";
import type { BottleneckCause, JobLive } from "../../lib/jobLive";

/** Catalog key and English fallback for each cause (written out so the i18n scripts see them). */
const CAUSE: Record<BottleneckCause, { key: string; text: string }> = {
  network: { key: "bottleneck_network", text: "network" },
  source: { key: "bottleneck_source", text: "source (archive decoding)" },
  disk: { key: "bottleneck_disk", text: "console disk" },
  workers: { key: "bottleneck_workers", text: "console workers" },
  memory: { key: "bottleneck_memory", text: "console memory" },
};

/** "Limited by: network", one line. Renders nothing without a cause. */
export function BottleneckLine({ cause }: { cause: BottleneckCause | null }) {
  const tr = useTr();
  if (!cause) return null;
  const name = tr(CAUSE[cause].key, undefined, CAUSE[cause].text);
  return (
    <div className="text-xs text-[var(--color-muted)]" data-testid="bottleneck-line">
      {tr("upload_bottleneck_label", { cause: name }, `Limited by: ${name}`)}
    </div>
  );
}

/** The skipping phase of a 7z/RAR resume: the decoder is reading past data the console
 *  already holds. Without this line the wait looks like a stall. */
export function SkippingLine({ live }: { live: JobLive }) {
  const tr = useTr();
  if (!live.skipping) return null;
  const done = formatBytes(live.skipDoneBytes);
  const total = formatBytes(live.skipTotalBytes);
  return (
    <div
      className="flex items-center gap-1.5 text-xs text-[var(--color-warn)]"
      data-testid="skipping-line"
    >
      <Loader2 size={12} className="animate-spin" aria-hidden />
      <span>
        {tr(
          "upload_phase_skipping",
          { done, total },
          `Skipping data the console already has: ${done} of ${total}`,
        )}
      </span>
    </div>
  );
}

/** Files are still settling on the console after the job finished. */
export function SettlingLine({ live }: { live: JobLive }) {
  const tr = useTr();
  if (!live.settling) return null;
  return (
    <div
      className="flex items-center gap-1.5 text-xs text-[var(--color-warn)]"
      data-testid="settling-line"
    >
      <Loader2 size={12} className="animate-spin" aria-hidden />
      <span>{tr("upload_phase_settling", undefined, "Finishing on the console…")}</span>
    </div>
  );
}

/** Everything a running job's live notes can say, in one block. Renders nothing when the
 *  engine sent no live fields. */
export function JobLiveNotes({ live }: { live: JobLive | undefined }) {
  if (!live) return null;
  return (
    <div className="mb-2 flex flex-col gap-0.5">
      <SkippingLine live={live} />
      <BottleneckLine cause={live.bottleneck} />
      <SettlingLine live={live} />
    </div>
  );
}
