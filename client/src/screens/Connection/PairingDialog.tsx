import { Loader2 } from "lucide-react";

import { Button } from "../../components";
import { Modal } from "../../components/Modal";
import { useTr } from "../../state/lang";
import { usePairingStore } from "../../state/pairing";
import type { PairingView } from "../../api/ava1";

export interface PairingPanelProps {
  view: PairingView | null;
  busy: boolean;
  error: string | null;
  onConfirm: () => void;
  onRetry: () => void;
  onCancel: () => void;
}

/** The body and buttons of the pairing dialog, separate from the store so it renders (and
 *  tests) without one. Three situations: a code to compare, a closed pairing window, and a
 *  console that could not be reached. */
export function PairingPanel({
  view,
  busy,
  error,
  onConfirm,
  onRetry,
  onCancel,
}: PairingPanelProps) {
  const tr = useTr();

  if (view?.state === "code") {
    const name = view.consoleName || "PS5";
    return (
      <>
        <div className="flex flex-col gap-3 p-4 text-sm">
          <p>
            {tr(
              "pairing_intro",
              { name },
              `${name} is asking to pair with this app. Check that the code below is the same one shown on your PS5, then confirm.`,
            )}
          </p>
          <div className="text-center">
            <div className="text-xs text-[var(--color-muted)]">
              {tr("pairing_code_label", undefined, "Pairing code")}
            </div>
            <div
              className="font-mono text-3xl font-semibold tracking-[0.3em] tabular-nums"
              data-testid="pairing-code"
            >
              {view.code}
            </div>
          </div>
          {error && (
            <p className="text-xs text-[var(--color-bad)]" role="alert">
              {tr("pairing_error", { error }, `Pairing failed: ${error}`)}
            </p>
          )}
        </div>
        <div className="flex items-center justify-end gap-2 border-t border-[var(--color-border)] px-4 py-3">
          <Button variant="ghost" onClick={onCancel}>
            {tr("pairing_codes_differ", undefined, "Codes differ")}
          </Button>
          <Button variant="primary" loading={busy} onClick={onConfirm}>
            {tr("pairing_confirm", undefined, "Codes match, pair")}
          </Button>
        </div>
      </>
    );
  }

  if (view?.state === "closed") {
    return (
      <>
        <div className="flex flex-col gap-2 p-4 text-sm">
          <p className="font-medium">
            {tr(
              "pairing_closed_title",
              undefined,
              "The PS5 is not accepting new pairings",
            )}
          </p>
          <p className="text-[var(--color-muted)]">
            {tr(
              "pairing_closed_body",
              undefined,
              "Its pairing window is closed. On a device that is already paired, open pairing for this console. If no device is paired yet, restart the helper on the console to reopen the window. Then try again.",
            )}
          </p>
        </div>
        <div className="flex items-center justify-end gap-2 border-t border-[var(--color-border)] px-4 py-3">
          <Button variant="ghost" onClick={onCancel}>
            {tr("close", undefined, "Close")}
          </Button>
          <Button variant="primary" loading={busy} onClick={onRetry}>
            {tr("pairing_retry", undefined, "Try again")}
          </Button>
        </div>
      </>
    );
  }

  // Loading, or the console could not be reached.
  return (
    <>
      <div className="flex flex-col gap-2 p-4 text-sm">
        {error ? (
          <p className="text-[var(--color-bad)]" role="alert">
            {tr("pairing_error", { error }, `Pairing failed: ${error}`)}
          </p>
        ) : (
          <p className="flex items-center gap-2 text-[var(--color-muted)]">
            <Loader2 size={14} className="animate-spin" aria-hidden />
            {tr("pairing_waiting", undefined, "Contacting the PS5…")}
          </p>
        )}
      </div>
      <div className="flex items-center justify-end gap-2 border-t border-[var(--color-border)] px-4 py-3">
        <Button variant="ghost" onClick={onCancel}>
          {tr("close", undefined, "Close")}
        </Button>
        {error && (
          <Button variant="primary" loading={busy} onClick={onRetry}>
            {tr("pairing_retry", undefined, "Try again")}
          </Button>
        )}
      </div>
    </>
  );
}

/** The dialog the store opens: when any call comes back not_paired, or from the Pair… button.
 *  Mounted once, in the app shell. The same engine routes serve the desktop, browser and
 *  Docker builds. */
export function PairingDialog() {
  const tr = useTr();
  const open = usePairingStore((s) => s.open);
  const view = usePairingStore((s) => s.view);
  const busy = usePairingStore((s) => s.busy);
  const error = usePairingStore((s) => s.error);
  const confirm = usePairingStore((s) => s.confirm);
  const retry = usePairingStore((s) => s.retry);
  const dismiss = usePairingStore((s) => s.dismiss);
  return (
    <Modal
      open={open}
      onClose={dismiss}
      title={tr("pairing_title", undefined, "Pair with your PS5")}
      size="sm"
      closeOnScrim={false}
    >
      <PairingPanel
        view={view}
        busy={busy}
        error={error}
        onConfirm={() => void confirm()}
        onRetry={() => void retry()}
        onCancel={dismiss}
      />
    </Modal>
  );
}
