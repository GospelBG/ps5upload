// The pairing dialog's state. The dialog opens when any call comes back `not_paired`
// (via `reportIfNotPaired`, wired in lib/invokeLogged and the upload poll) or when the user
// presses Pair… on the status pill. The engine holds the handshake whose code is on screen;
// this store only reads it and relays the user's "the codes match".
//
// The app's own launches never reach it: a helper this app sent pairs with no code.

import { create } from "zustand";

import {
  pairingConfirm,
  pairingStatus,
  type PairingView,
} from "../api/ava1";
import { hostOf } from "../lib/addr";
import { onNotPaired } from "../lib/consoleSession";
import { useConnectionStore } from "./connection";

/** After the user dismisses the dialog for a console, an automatic open for it waits this long. */
const QUIET_MS = 60_000;

interface PairingState {
  open: boolean;
  host: string;
  /** The last thing the engine said; null while loading or after a transport error. */
  view: PairingView | null;
  busy: boolean;
  error: string | null;
  /** The console that was just paired (the status poll flips its pill on its next tick). */
  paired: string | null;
  /** Per console: no automatic open before this wall-clock ms. */
  quietUntil: Record<string, number>;
  /** Opens for `host` (default: the active console) and starts/re-reads the handshake. */
  openFor: (host?: string) => Promise<void>;
  /** Asks again (after the user opened the pairing window on the console). */
  retry: () => Promise<void>;
  confirm: () => Promise<void>;
  /** Closes the dialog. */
  close: () => void;
  /** Closes it because the user said no: automatic opens for this console go quiet. */
  dismiss: () => void;
}

export const usePairingStore = create<PairingState>((set, get) => {
  const load = async (host: string) => {
    set({ busy: true, error: null });
    try {
      const view = await pairingStatus(host);
      // Already paired (the console trusts us after all): nothing to compare.
      if (view.state === "accepted") {
        set({ busy: false, view, open: false, paired: hostOf(host) });
        return;
      }
      set({ busy: false, view });
    } catch (e) {
      set({
        busy: false,
        view: null,
        error: e instanceof Error ? e.message : String(e),
      });
    }
  };

  return {
    open: false,
    host: "",
    view: null,
    busy: false,
    error: null,
    paired: null,
    quietUntil: {},

    async openFor(host) {
      const h = (host ?? useConnectionStore.getState().host).trim();
      if (!h) return;
      set({ open: true, host: h, view: null, error: null, paired: null });
      await load(h);
    },

    async retry() {
      const { host } = get();
      if (host) await load(host);
    },

    async confirm() {
      const { host, view } = get();
      if (!host || view?.state !== "code") return;
      set({ busy: true, error: null });
      try {
        const next = await pairingConfirm(host);
        if (next.state === "accepted") {
          set({ busy: false, open: false, view: next, paired: hostOf(host) });
        } else {
          // `closed`: the console's window shut before we confirmed.
          set({ busy: false, view: next });
        }
      } catch (e) {
        set({
          busy: false,
          error: e instanceof Error ? e.message : String(e),
        });
      }
    },

    close() {
      set({ open: false, view: null, error: null, busy: false });
    },

    dismiss() {
      const key = hostOf(get().host) || "_";
      set((s) => ({
        open: false,
        view: null,
        error: null,
        busy: false,
        quietUntil: { ...s.quietUntil, [key]: Date.now() + QUIET_MS },
      }));
    },
  };
});

// Any call that comes back not_paired opens the dialog, unless it is already open or
// the user just dismissed it for this console.
onNotPaired((host) => {
  const s = usePairingStore.getState();
  if (s.open) return;
  const h = (host ?? useConnectionStore.getState().host).trim();
  const key = hostOf(h) || "_";
  if (Date.now() < (s.quietUntil[key] ?? 0)) return;
  void s.openFor(h);
});
