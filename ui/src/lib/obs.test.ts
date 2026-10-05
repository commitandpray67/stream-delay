import { describe, expect, it } from "vitest";
import { OverlayReporter } from "./obs";

function setup() {
  const sent: object[] = [];
  const frames: Array<() => void> = [];
  const r = new OverlayReporter(
    (m) => sent.push(m),
    (cb) => frames.push(cb),
  );
  const nextFrame = () => frames.splice(0).forEach((cb) => cb());
  return { r, sent, nextFrame };
}

const obsEvent = (active: boolean) => new CustomEvent("obsSourceActiveChanged", { detail: { active } });

describe("OverlayReporter", () => {
  it("says nothing about being on stream until OBS does", () => {
    const { r, sent } = setup();
    const obs = new EventTarget();
    r.listen(obs);
    r.connected();
    expect(sent).toEqual([]);
    obs.dispatchEvent(obsEvent(true));
    expect(sent).toEqual([{ type: "overlay", active: true }]);
    obs.dispatchEvent(obsEvent(false));
    expect(sent.at(-1)).toEqual({ type: "overlay", active: false });
    // Said again on every reconnection.
    sent.length = 0;
    r.connected();
    expect(sent).toEqual([{ type: "overlay", active: false }]);
  });

  it("confirms the slate again after a reconnection, which may be to a restarted stream-delay", () => {
    const { r, sent, nextFrame } = setup();
    r.slateShown(3);
    nextFrame();
    nextFrame();
    expect(sent).toEqual([{ type: "slate-shown", change: 3 }]);
    // stream-delay restarted while OBS kept the page open: it numbers its
    // changes from the start again, and has not been told about this one.
    r.connected();
    r.slateShown(3);
    nextFrame();
    nextFrame();
    expect(sent).toEqual([
      { type: "slate-shown", change: 3 },
      { type: "slate-shown", change: 3 },
    ]);
  });

  it("does not confirm a slate viewers cannot see: off stream, it waits until OBS puts the page on", () => {
    const { r, sent, nextFrame } = setup();
    const obs = new EventTarget();
    r.listen(obs);
    obs.dispatchEvent(obsEvent(false));
    sent.length = 0;
    r.slateShown(5);
    nextFrame();
    nextFrame();
    expect(sent).toEqual([]);
    // Now viewers see it.
    obs.dispatchEvent(obsEvent(true));
    expect(sent).toEqual([
      { type: "overlay", active: true },
      { type: "slate-shown", change: 5 },
    ]);
    // Once.
    obs.dispatchEvent(obsEvent(false));
    obs.dispatchEvent(obsEvent(true));
    expect(sent.filter((m) => (m as { type: string }).type === "slate-shown")).toHaveLength(1);
  });

  it("confirms the slate only once it has been painted, once per change", () => {
    const { r, sent, nextFrame } = setup();
    r.slateShown(3);
    expect(sent).toEqual([]);
    nextFrame();
    expect(sent).toEqual([]);
    nextFrame();
    expect(sent).toEqual([{ type: "slate-shown", change: 3 }]);
    r.slateShown(3);
    nextFrame();
    nextFrame();
    expect(sent).toHaveLength(1);
    r.slateShown(4);
    nextFrame();
    nextFrame();
    expect(sent.at(-1)).toEqual({ type: "slate-shown", change: 4 });
  });
});
