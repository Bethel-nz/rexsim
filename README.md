# rexsim

A live iOS Simulator inside a [Rex](https://www.superlogical.com/rex) pane.

Run `rexsim` in a Rex terminal and a pane opens beside you with the simulator's screen in
it. Click, drag, scroll and type in the pane to use the phone. Your AI agents can keep
driving the same simulator with the [`simdriver`](https://github.com/Bethel-nz/simdriver)
CLI at the same time, and the pane shows everything they do.

```
cargo install --git https://github.com/Bethel-nz/rexsim
xcrun simctl boot "iPhone 18 Pro"    # or boot one from Xcode
rexsim
```

## Options

```
rexsim [--udid <udid>] [--fps N] [--split right|left|above|below] [--here] [--log <file>]
```

| Flag | Meaning |
| --- | --- |
| `--udid <udid>` | Which simulator. Default: the first booted one. |
| `--fps N` | Frame rate cap. Default 60. |
| `--split DIR` | Where the new pane opens. Default `right`. |
| `--here` | Draw in the current terminal instead of opening a pane. |
| `--no-stick` | Let the pane be any width. By default it keeps to the phone's width. |
| `--log <file>` | Write a frame-by-frame timing log. Also `$REXSIM_LOG`. |

## In the pane

- **Touch:** click, drag and scroll on the phone. Type to send keys.
- **Hardware buttons:** the toolbar under the phone has Home, Apps (the app switcher),
  Lock, Siri, and volume down and up. Click for a press; hold for more than half a second
  for a long press of that length, like a physical button.
- **Width:** the pane sizes itself to fit the phone and snaps back to that width when the
  window or the pane next to it changes size. Use `--no-stick` to turn this off.
- **Keys:** Ctrl-H presses Home. Ctrl-C quits.

## How it works

- **Frames.** [simdriver](https://github.com/Bethel-nz/simdriver) streams the simulator's
  framebuffer as raw frames. rexsim draws each one with the Kitty graphics protocol,
  which Rex supports through libghostty.
- **Shared memory.** Each frame is written to a POSIX shared memory segment, and only the
  segment's name is sent through the terminal. A full frame sent as inline base64 manages
  about 10 fps; shared memory keeps up with the simulator.
- **No flashing.** Frames alternate between two image ids. Each new frame is placed, and
  the previous one deleted, in one synchronized update, so the picture never blanks.
- **No backlog.** One frame is in flight at a time. The next goes out when Rex
  acknowledges the last, and the newest frame wins, so a slow moment drops frames
  instead of queueing them.
- **Input.** Mouse reporting in pixel mode (1016) maps a click to an exact point on the
  device screen, not a whole terminal cell.
- **Status.** The pane reports its state to Rex with the
  [Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status)
  (OSC 7501), so the tab shows when it's connecting, live, or stopped.

## Status

Early. Peaks around 30–40 fps. Each full frame is about 3 MB, and Rex sometimes takes
50–400 ms to acknowledge one, which shows as stutter. Sending only the parts of the screen
that changed is next.

Requires macOS on Apple silicon, Xcode, and Rex.

## License

MIT
