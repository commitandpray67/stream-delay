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
 *
 * Off stream, viewers do not see the slate: the page confirms it once OBS puts
 * the page on stream, not before.
 */
export class OverlayReporter {
  private active: boolean | null = null;
  private confirmed: number | null = null;
  /** The slate painted while off stream, confirmed when the page goes on. */
  private waiting: number | null = null;

  constructor(
    private readonly send: Send,
    private readonly frame: Frame = (cb) => requestAnimationFrame(cb),
  ) {}

  /** Listens to what OBS says (on `window` in a browser source). Returns what stops it. */
  listen(target: EventTarget): () => void {
    const changed = (e: Event) => {
      this.active = (e as CustomEvent<{ active?: unknown }>).detail?.active === true;
      this.report();
      if (this.active && this.waiting !== null) {
        const change = this.waiting;
        this.waiting = null;
        this.send({ type: "slate-shown", change });
      }
    };
    target.addEventListener("obsSourceActiveChanged", changed);
    return () => target.removeEventListener("obsSourceActiveChanged", changed);
  }

  /**
   * Call on every (re)connection to stream-delay. It may have restarted, and
   * numbers the slate's changes from the start again: the slate up now is
   * confirmed anew.
   */
  connected(): void {
    this.confirmed = null;
    this.waiting = null;
    this.report();
  }

  /**
   * Call once the slate for `change` is in the page: after two frames it has
   * been painted, and the page says so, once per change (off stream, once it
   * is on).
   */
  slateShown(change: number): void {
    if (this.confirmed === change) return;
    this.confirmed = change;
    this.frame(() =>
      this.frame(() => {
        if (this.active === false) this.waiting = change;
        else this.send({ type: "slate-shown", change });
      }),
    );
  }

  private report(): void {
    if (this.active !== null) this.send({ type: "overlay", active: this.active });
  }
}
