// What the overlay page tells stream-delay about itself inside OBS.

/** True inside an OBS browser source. */
export function inObs(): boolean {
  return typeof (globalThis as { obsstudio?: unknown }).obsstudio !== "undefined";
}

type Send = (msg: object) => void;
type Frame = (callback: () => void) => void;

/**
 * Reports what OBS says about this overlay page: whether it is on stream (in
 * the program output), and when it has painted the slate for a change.
 *
 * OBS says when the page goes on or off stream, but not whether it is when the
 * page loads (after OBS starts, or the page is refreshed). Until it says, the
 * page reports nothing, and does not count as covering the stream: hiding and
 * showing it once in OBS lets it know.
 */
export class OverlayReporter {
  private active: boolean | null = null;
  private confirmed: number | null = null;

  constructor(
    private readonly send: Send,
    private readonly frame: Frame = (cb) => requestAnimationFrame(cb),
  ) {}

  /** Listens to what OBS says (on `window` in a browser source). */
  listen(target: EventTarget): void {
    target.addEventListener("obsSourceActiveChanged", (e) => {
      this.active = (e as CustomEvent<{ active?: unknown }>).detail?.active === true;
      this.report();
    });
  }

  /** Call on every (re)connection to stream-delay. */
  connected(): void {
    this.report();
  }

  /**
   * Call once the slate for `change` is in the page: after two frames it has
   * been painted, and the page says so, once per change.
   */
  slateShown(change: number): void {
    if (this.confirmed === change) return;
    this.confirmed = change;
    this.frame(() => this.frame(() => this.send({ type: "slate-shown", change })));
  }

  private report(): void {
    if (this.active !== null) this.send({ type: "overlay", active: this.active });
  }
}
