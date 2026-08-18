# Placing a window that argues back

Moving a window is not a request that either succeeds or fails. It is a request
the application gets a say in, and on a desktop with displays at different
scaling it gets that say *after* the move has already returned.

## What went wrong

Reported after a live production on v0.1.20: sending Zoom's video window to the
larger display and retrieving it brought the window back far bigger than it
started, big enough to be disruptive on the display it came back to.

The mechanism is Windows' per-monitor DPI, and it has two halves.

**The application is told late.** Zoom's video window is per-monitor-DPI-aware.
Moving it between displays of different scaling does not resize it during
`SetWindowPos`; Windows sends the window `WM_DPICHANGED` with a suggested
rectangle scaled by the ratio between the two displays, and the application
resizes itself to that — on its own thread, after our call has returned. A
window restored to 1280x800 on a display at 150% is holding 1920x1200 a moment
later.

**Measuring immediately measures the gap.** `place_window` asked, measured, and
corrected once, all synchronously. That measurement lands in the window between
the move and the application's response, where the window is exactly the size
it was asked to be and there is nothing to correct. Which is also why the fault
looked intermittent: when Zoom's thread happened to process the scaling change
before the measurement, the existing correction cancelled it exactly and the
restore looked fine.

## The fix, in two parts

### A bound on the correction

`corrected_request` in `src/platform.rs` cancels a scale factor by asking for
the square of the request over the result. That is right, and unbounded: a
window that lands at half the size asked for makes the next ask twice as large.
On Send the first ask is already the whole display, so a second ask could be two
displays wide and spill onto the neighbour. The correction is now clamped to the
display it is aimed at — it may not ask for more than that monitor, or for an
origin off it.

The bound applies to the correction only. The original request passes through
untouched, so a window the user had straddling two displays is still placed
straddling them; it is only the guess at what to ask for *instead* that is kept
on one screen.

### A watch, because one measurement is not an answer

`Core` now watches the last placement for a beat after making it, on the same
split as the fade and the full-screen watch: `Core` measures and corrects,
`app.rs` owns the clock. One look every 50ms for 1.2 seconds.

Each look is a measurement:

- **Where it was put** — nothing to do.
- **Somewhere else, seen once** — a window caught mid-move has not drifted.
  Look again. Correcting here spends a correction on nothing.
- **Somewhere else, the same somewhere twice in a row** — it has come to rest
  in the wrong place. Re-assert the placement, verbatim.
- **Still wrong after two corrections** — say so and stop.

The re-assertion is the same request again rather than a cleverer one, and that
is the point: by the time it is made the window is already on the destination
display, so there is no scaling boundary left to cross and nothing to
compensate for. The first placement is the only one that can be undone this
way, which is why one correction is normally enough.

Two corrections is a cap, not a convergence loop. Past it the application is
not losing an argument, it is having one, and a window flickering between two
sizes on camera is worse than a window that is the wrong size and said so.

The watch runs its full window rather than stopping at the first agreeable
measurement. Stopping early would mean stopping in exactly the gap the bug
lives in — reading the one moment the window is still the size it was given.

Nothing is reported when a placement holds. "Sent to DISPLAY2" already said what
happened, and a second line confirming that a window is the size it was asked to
be is noise on a strip with room for three messages.

## Why not predict the scaling instead

The rejected alternative: record the DPI of the display a window came from, work
out the ratio to the display it is going back to, and pre-divide the request so
the application's own rescale lands on the right size.

It would avoid the correction being visible at all, and it is a guess about
another process. Applications differ in what they do with `WM_DPICHANGED` —
honour the suggested rectangle, ignore it, or clamp it against their own minimum
size — and a guess that is wrong makes the window wrong in a way no measurement
afterwards would catch, because we would have stopped looking. The watch is
slower and it is a reading.

## What the diagnostics now say

Two additions, both aimed at the next live run rather than at a mock:

- Every display's DPI and scaling percentage, and an explicit note when they
  differ, since that is the condition the whole of the above exists for.
- `LAST PLACEMENT`: what was asked for, what it landed at, how many corrections
  it took, and whether it ever settled. The one question a screenshot of the
  desktop cannot answer after a window has come back the wrong size.

## What a real desktop still has to answer

The mock models a window that resizes itself by the scaling ratio one look after
being placed, which is what makes the fault reproducible on a Mac. What it
cannot model is Zoom:

- Whether Zoom's video window honours the re-assertion at all, or clamps it
  against a minimum size of its own.
- Whether 1.2 seconds covers Zoom's response. A window that inflates at 1.5
  seconds is a window this watch has stopped looking at.
- Whether stripping the frame for a borderless fill and putting it back
  interacts with the scaling change, since both arrive as the same
  `SWP_FRAMECHANGED`.

`LAST PLACEMENT` in the diagnostics answers all three from a real session.
