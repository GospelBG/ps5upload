import { useState } from "react";
import { KeyRound, RefreshCw } from "lucide-react";

import { Button } from "../components";
import { ensurePayloadCurrent } from "../lib/ensurePayloadCurrent";
import { useConnectionStore } from "../state/connection";
import { useTr } from "../state/lang";
import { usePairingStore } from "../state/pairing";
import { hostOf } from "../lib/addr";
import { isLegacyHelperWedged, type SessionState } from "../lib/consoleSession";
import { replaceHelper } from "../api/ava1";

export interface SessionBannerViewProps {
  session: SessionState | null;
  /** The old helper would not exit: only a console restart clears it. */
  wedged: boolean;
  busy: boolean;
  onPair: () => void;
  onUpdate: () => void;
}

/** What the one status probe asks of the person: pair, or update the helper. Nothing for
 *  connected or down (down has the Send helper flow on the Connection screen). */
export function SessionBannerView({
  session,
  wedged,
  busy,
  onPair,
  onUpdate,
}: SessionBannerViewProps) {
  const tr = useTr();
  if (session === "needs_pairing") {
    return (
      <div className="border-b border-[var(--color-border)] bg-[var(--color-warn-soft)] px-3 py-2 text-[var(--color-text)]">
        <div className="mx-auto flex max-w-6xl flex-col gap-2 sm:flex-row sm:items-center sm:gap-3">
          <div className="flex min-w-0 flex-1 items-start gap-3">
            <KeyRound size={18} className="mt-0.5 shrink-0 text-[var(--color-warn)]" />
            <p className="text-sm font-medium">
              {tr(
                "session_needs_pairing",
                undefined,
                "This PS5 has not accepted this app yet. Pair them to continue.",
              )}
            </p>
          </div>
          <Button size="sm" variant="primary" onClick={onPair}>
            {tr("session_pair_button", undefined, "Pair…")}
          </Button>
        </div>
      </div>
    );
  }
  if (session === "helper_old") {
    return (
      <div className="border-b border-[var(--color-border)] bg-[var(--color-warn-soft)] px-3 py-2 text-[var(--color-text)]">
        <div className="mx-auto flex max-w-6xl flex-col gap-2 sm:flex-row sm:items-center sm:gap-3">
          <div className="flex min-w-0 flex-1 items-start gap-3">
            <RefreshCw size={18} className="mt-0.5 shrink-0 text-[var(--color-warn)]" />
            <div className="min-w-0">
              <p className="text-sm font-medium">
                {tr(
                  "helper_old_banner",
                  undefined,
                  "This PS5 is running an older helper. Update it.",
                )}
              </p>
              {wedged && (
                <p className="text-xs text-[var(--color-muted)]">
                  {tr(
                    "helper_old_wedged",
                    undefined,
                    "The old helper did not exit when asked. Restart the console, then update the helper.",
                  )}
                </p>
              )}
            </div>
          </div>
          {!wedged && (
            <Button size="sm" variant="primary" loading={busy} onClick={onUpdate}>
              {tr("helper_old_update", undefined, "Update helper")}
            </Button>
          )}
        </div>
      </div>
    );
  }
  return null;
}

/** The active console's banner. The update is the same one-click send the Connection
 *  screen uses (the engine shuts the old helper down first, then sends the new one stamped
 *  with this engine's key, so no pairing code appears). */
export default function SessionBanner() {
  const host = useConnectionStore((s) => s.host);
  const rt = useConnectionStore(
    (s) => s.runtimeByHost[hostOf(host) || "_"],
  );
  const [busy, setBusy] = useState(false);
  const openPairing = usePairingStore((s) => s.openFor);
  if (!host.trim() || !rt) return null;
  return (
    <SessionBannerView
      session={rt.session}
      wedged={rt.helperWedged}
      busy={busy}
      onPair={() => void openPairing(host)}
      onUpdate={() => {
        setBusy(true);
        // The engine's replace flow: old helper's shutdown, stamped helper, wait for the port.
        void replaceHelper(host)
          .catch((e: unknown) => {
            if (isLegacyHelperWedged(e)) {
              useConnectionStore.getState().setHostStatus(host, { helperWedged: true });
              return;
            }
            // No helper running at all: the ordinary send flow.
            return ensurePayloadCurrent(host, undefined, true).then(() => {});
          })
          .finally(() => setBusy(false));
      }}
    />
  );
}
