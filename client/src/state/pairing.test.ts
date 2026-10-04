import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  pairingStatus: vi.fn(),
  pairingConfirm: vi.fn(),
}));
vi.mock("../api/ava1", () => api);

import { reportIfNotPaired } from "../lib/consoleSession";
import { usePairingStore } from "./pairing";

beforeEach(() => {
  api.pairingStatus.mockReset();
  api.pairingConfirm.mockReset();
  usePairingStore.getState().close();
  usePairingStore.setState({ quietUntil: {} });
});

const CODE = { state: "code", code: "004821", consoleName: "PS5-Pro" } as const;

describe("pairing store", () => {
  it("shows the code the console shows, then confirms", async () => {
    api.pairingStatus.mockResolvedValue(CODE);
    api.pairingConfirm.mockResolvedValue({ state: "accepted" });
    await usePairingStore.getState().openFor("10.0.0.2");
    let s = usePairingStore.getState();
    expect(s.open && s.host).toBe("10.0.0.2");
    expect(s.view).toEqual(CODE);
    await s.confirm();
    expect(api.pairingConfirm).toHaveBeenCalledWith("10.0.0.2");
    s = usePairingStore.getState();
    expect(s.open).toBe(false);
    expect(s.paired).toBe("10.0.0.2");
  });

  it("explains a closed window and can try again", async () => {
    api.pairingStatus.mockResolvedValueOnce({ state: "closed" });
    await usePairingStore.getState().openFor("10.0.0.2");
    expect(usePairingStore.getState().view).toEqual({ state: "closed" });
    api.pairingStatus.mockResolvedValueOnce(CODE);
    await usePairingStore.getState().retry();
    expect(usePairingStore.getState().view).toEqual(CODE);
  });

  it("a confirm that finds the window shut goes back to the closed explanation", async () => {
    api.pairingStatus.mockResolvedValue(CODE);
    api.pairingConfirm.mockResolvedValue({ state: "closed" });
    await usePairingStore.getState().openFor("10.0.0.2");
    await usePairingStore.getState().confirm();
    const s = usePairingStore.getState();
    expect(s.open).toBe(true);
    expect(s.view).toEqual({ state: "closed" });
  });

  it("keeps the dialog open with the error when the console cannot be reached", async () => {
    api.pairingStatus.mockRejectedValue(new Error("timed out"));
    await usePairingStore.getState().openFor("10.0.0.2");
    const s = usePairingStore.getState();
    expect(s.open).toBe(true);
    expect(s.error).toBe("timed out");
    expect(s.view).toBeNull();
  });

  it("opens by itself when any call comes back not_paired, once", async () => {
    api.pairingStatus.mockResolvedValue(CODE);
    expect(reportIfNotPaired("ava1_not_paired", "10.0.0.2")).toBe(true);
    await vi.waitFor(() => expect(usePairingStore.getState().open).toBe(true));
    expect(api.pairingStatus).toHaveBeenCalledTimes(1);
    // A second not_paired while it is open does not start a second handshake.
    reportIfNotPaired("not_paired", "10.0.0.2");
    expect(api.pairingStatus).toHaveBeenCalledTimes(1);
  });

  it("stays quiet for a while after the user dismisses it", async () => {
    api.pairingStatus.mockResolvedValue(CODE);
    await usePairingStore.getState().openFor("10.0.0.2");
    usePairingStore.getState().dismiss();
    reportIfNotPaired("not_paired", "10.0.0.2");
    await Promise.resolve();
    expect(usePairingStore.getState().open).toBe(false);
    // The explicit Pair… button is never muted.
    await usePairingStore.getState().openFor("10.0.0.2");
    expect(usePairingStore.getState().open).toBe(true);
  });

  it("never opens for a failure that names no console, and opens the named one, not the active one", async () => {
    api.pairingStatus.mockResolvedValue(CODE);
    reportIfNotPaired("not_paired");
    await Promise.resolve();
    expect(usePairingStore.getState().open).toBe(false);
    expect(api.pairingStatus).not.toHaveBeenCalled();
    reportIfNotPaired("not_paired", "10.0.0.9");
    await vi.waitFor(() => expect(usePairingStore.getState().open).toBe(true));
    expect(usePairingStore.getState().host).toBe("10.0.0.9");
    expect(api.pairingStatus).toHaveBeenCalledWith("10.0.0.9");
  });

  it("treats a state it does not know as an error the user can retry", async () => {
    api.pairingStatus.mockResolvedValue({ state: "none" });
    await usePairingStore.getState().openFor("10.0.0.2");
    const s = usePairingStore.getState();
    expect(s.open).toBe(true);
    expect(s.view).toBeNull();
    expect(s.error).toContain("unexpected");
    api.pairingStatus.mockResolvedValue(CODE);
    await s.retry();
    expect(usePairingStore.getState().view).toEqual(CODE);
    expect(usePairingStore.getState().error).toBeNull();
  });
});
