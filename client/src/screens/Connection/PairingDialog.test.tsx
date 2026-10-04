import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { PairingPanel } from "./PairingDialog";

const noop = () => {};
const panel = (over: Partial<Parameters<typeof PairingPanel>[0]>) =>
  renderToStaticMarkup(
    <PairingPanel
      view={null}
      busy={false}
      error={null}
      onConfirm={noop}
      onRetry={noop}
      onCancel={noop}
      {...over}
    />,
  );

describe("pairing_dialog_shows_the_code_and_confirms", () => {
  it("shows the six digits, the console's name and a confirm button", () => {
    const html = panel({
      view: { state: "code", code: "004821", consoleName: "PS5-Pro" },
    });
    expect(html).toContain("004821");
    expect(html).toContain("PS5-Pro is asking to pair");
    expect(html).toContain("Codes match, pair");
    expect(html).toContain("Codes differ");
  });

  it("explains how to reopen a closed pairing window and offers a retry", () => {
    const html = panel({ view: { state: "closed" } });
    expect(html).toContain("The PS5 is not accepting new pairings");
    expect(html).toContain("already paired");
    expect(html).toContain("restart the helper");
    expect(html).toContain("Try again");
    expect(html).not.toContain("Codes match");
  });

  it("says when the console could not be reached", () => {
    const html = panel({ view: null, error: "timed out" });
    expect(html).toContain("Pairing failed: timed out");
    expect(html).toContain("Try again");
  });

  it("shows progress while the first answer is pending", () => {
    expect(panel({ view: null })).toContain("Contacting the PS5");
  });
});
