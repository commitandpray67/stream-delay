// User-facing strings. Add a language by adding a dictionary with the same keys.

const en = {
  "phase.offline": "Offline",
  "phase.live": "No delay",
  "phase.delayed": "Delayed {delay}",
  "phase.adding": "Adding delay…",
  "phase.going-live": "Removing delay…",
  "phase.reducing": "Changing delay…",
  "phase.holding": "Holding the picture…",
  "phase.ending": "Ending stream…",
  "phase.ended": "Stream ended",
  "hint.offline": "Start streaming in OBS to begin.",
  "hint.live": "Viewers see you in real time.",
  "hint.delayed": "Viewers see you {delay} behind.",
  "hint.adding": "The overlay slate is covering the stream while the delay builds.",
  "hint.going-live": "Waiting for the next keyframe, or for what is buffered to air.",
  "hint.reducing": "Waiting for a keyframe to shorten the delay.",
  "hint.holding": "What viewers hadn't seen was thrown away. They see the last frame, still, until the delay is back.",
  "hint.ending": "What viewers haven't seen yet airs, then the broadcast ends. Nothing after your click airs.",
  "hint.ended": "Nothing is being sent. To go live again, stop and start streaming in OBS.",
  "mode.rewind": "Rewind",
  "mode.rewind.help": "Instant. Viewers see the last few seconds again.",
  "mode.mask": "Mask",
  "mode.mask.help": "Shows the overlay slate first, so nothing repeats. Takes about twice the delay.",
  "mode.maskOnly": "Delay is added behind the overlay slate (Mask), because the rolling buffer is off in Setup.",
  "action.removeDelayAfter": "Remove delay after it airs",
  "action.removeDelayAfter.help":
    "Viewers still see everything up to this moment, then the delay is removed. What happens while that airs is skipped.",
  "action.dump": "Dump buffer",
  "action.dump.confirm": "Click again to dump",
  "action.dump.help":
    "Throws away what viewers haven't seen yet, so it never airs, and keeps streaming with the same delay. For the moment something happens that must not go out.",
  "action.dump.replay": "Viewers see the last {delay} again, then the stream continues from after the dump.",
  "action.dump.slate": "The overlay slate covers the stream for about {delay} while the delay builds back up.",
  "action.dump.slate.muted": "Your sound is left out until then.",
  "action.dump.slate.sound": "Your sound still airs under it.",
  "action.dump.hold":
    "Nothing would cover the stream, so viewers see the last frame they saw, still and silent, for about {delay}. Then the stream continues with the delay.",
  "action.dump.hold.noOverlay": "No overlay is connected.",
  "action.dump.hold.unconfirmed":
    "The overlay hasn't said it's on stream: after OBS starts, hide and show it once in OBS (the eye icon) so it can.",
  "dumped.replay": "Dumped. Viewers see the last {delay} again.",
  "dumped.cover": "Dumped. The slate covers the stream while the delay builds back up.",
  "dumped.hold": "Dumped. Viewers see a still frame until the delay is back.",
  "dumped.gone": "Dumped. Nothing that was waiting will air.",
  "action.dump.noDelay": "There is no delay, so nothing is waiting to air.",
  "action.endStream": "End stream",
  "action.endStream.confirm": "Click again to end",
  "action.endStream.help":
    "Airs what viewers haven't seen yet, then ends the broadcast. Nothing after your click airs.",
  "action.endNow": "End stream now",
  "action.endNow.confirm": "Click again to end now",
  "action.endNow.help": "Ends the broadcast at once. What viewers haven't seen yet is thrown away and never airs.",
  "action.endNow.warning":
    "End stream now cuts the broadcast off at once. The last {delay} that viewers haven't seen yet is thrown away and never airs. To air it first, use End stream instead.",
  "action.keepStreaming": "Keep streaming",
  "action.resume": "Resume broadcasting",
  "action.resume.help": "Starts a new broadcast from what OBS sends from now on, without restarting the stream in OBS.",
  "action.cancel": "Cancel",
  "action.backTo": "Back to {delay}",
  "action.set": "Set",
  "popup.dontShow": "Don't show this again",
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
