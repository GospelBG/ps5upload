import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

import { BottleneckLine, JobLiveNotes, UnsettledLine } from "./Bottleneck";
import type { BottleneckCause, JobLive } from "../../lib/jobLive";

const live = (over: Partial<JobLive>): JobLive => ({
  skipping: false,
  skipDoneBytes: 0,
  skipTotalBytes: 0,
  bottleneck: null,
  settling: false,
  ...over,
});

describe("bottleneck_line_renders_each_cause", () => {
  const cases: Array<[BottleneckCause, string]> = [
    ["network", "Limited by: network"],
    ["source", "Limited by: source (archive decoding)"],
    ["disk", "Limited by: console disk"],
    ["workers", "Limited by: console workers"],
    ["memory", "Limited by: console memory"],
  ];
  it.each(cases)("%s", (cause, text) => {
    expect(renderToStaticMarkup(<BottleneckLine cause={cause} />)).toContain(text);
  });
  it("renders nothing without a cause", () => {
    expect(renderToStaticMarkup(<BottleneckLine cause={null} />)).toBe("");
  });
});

describe("skipping_phase_renders", () => {
  it("says what is being skipped, with both byte counts", () => {
    const html = renderToStaticMarkup(
      <JobLiveNotes
        live={live({ skipping: true, skipDoneBytes: 1024 ** 3, skipTotalBytes: 4 * 1024 ** 3 })}
      />,
    );
    expect(html).toContain("Skipping data the console already has");
    expect(html).toContain("1.00 GiB");
    expect(html).toContain("4.00 GiB");
    expect(html).toContain('data-testid="skipping-line"');
  });

  it("shows nothing when the engine sent no live fields", () => {
    expect(renderToStaticMarkup(<JobLiveNotes live={undefined} />)).toBe("");
  });
});

describe("finishing on the console", () => {
  it("shows only when settling is set", () => {
    expect(renderToStaticMarkup(<JobLiveNotes live={live({ settling: true })} />)).toContain(
      "Finishing on the console…",
    );
    expect(
      renderToStaticMarkup(<JobLiveNotes live={live({ bottleneck: "network" })} />),
    ).not.toContain("Finishing on the console");
  });
});

describe("a finished upload the console did not confirm", () => {
  it("is a warning in the result, not a clean success", () => {
    const html = renderToStaticMarkup(<UnsettledLine live={live({ unsettled: true })} />);
    expect(html).toContain('data-testid="unsettled-warning"');
    expect(html).toContain("has not confirmed saving all files");
  });
  it("shows nothing for a clean finish or no notes", () => {
    expect(renderToStaticMarkup(<UnsettledLine live={live({})} />)).toBe("");
    expect(renderToStaticMarkup(<UnsettledLine live={undefined} />)).toBe("");
  });
});
