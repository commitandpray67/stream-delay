// User-facing strings. Add a language by adding a dictionary with the same keys.

const en = {
  "phase.offline": "Offline",
  "phase.live": "Live",
  "phase.delayed": "Delayed {delay}",
  "phase.adding": "Adding delay…",
  "phase.going-live": "Going live…",
  "phase.reducing": "Changing delay…",
  "phase.ended": "Stream ended",
  "hint.offline": "Start streaming in OBS to begin.",
  "hint.live": "Viewers see you in real time.",
  "hint.delayed": "Viewers see you {delay} behind.",
  "hint.adding": "The overlay slate is covering the stream while the delay builds.",
  "hint.going-live": "Waiting for the next keyframe or for buffered content to air.",
  "hint.reducing": "Waiting for a keyframe to shorten the delay.",
  "hint.ended": "Nothing is being sent. Resume, or stop and start the stream in OBS.",
  "mode.rewind": "Rewind",
  "mode.rewind.help": "Instant. Viewers see the last few seconds again.",
  "mode.mask": "Mask",
  "mode.mask.help": "Shows the overlay slate first, so nothing repeats. Takes about twice the delay.",
  "mode.maskOnly": "Delay is added behind the overlay slate (Mask), because the rolling buffer is off in Setup.",
  "action.goLive": "Go live now",
  "action.goLiveAfter": "Air up to now, then go live",
  "action.goLiveAfter.help":
    "Viewers still see everything up to this moment, then jump to live. What happens while that airs is skipped.",
  "action.endStream": "End stream",
  "action.endStream.confirm": "Click again to end",
  "action.endStream.help": "Ends the broadcast now. Nothing still in the delay buffer is shown.",
  "action.resume": "Resume broadcasting",
  "action.cancel": "Cancel",
  "action.set": "Set",
  "custom.label": "Custom delay (seconds)",
  "buffer.label": "Buffered {history} of {max}",
  "buffer.labelNoHistory": "Buffered {history}",
  "conn.lost": "Connection to stream-delay lost. Reconnecting…",
  "conn.unauthorized": "This link has no valid token. Copy the dock or dashboard link again from stream-delay.",
} as const;

export type Key = keyof typeof en;
const dictionaries: Record<string, Record<Key, string>> = { en };

const lang = (() => {
  const l = (typeof navigator !== "undefined" ? navigator.language : "en").split("-")[0];
  return dictionaries[l] ? l : "en";
})();

export function t(key: Key, vars: Record<string, string | number> = {}): string {
  const s: string = dictionaries[lang][key] ?? en[key];
  return s.replace(/\{(\w+)\}/g, (_, k: string) => String(vars[k] ?? `{${k}}`));
}
