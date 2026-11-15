# VWM progress

This file says where the work on invisible windows stands. Whoever continues reads it first,
then `DESIGN.md`, and carries on at "Next action".

## Rules

- Work on branch `vwm`. Small commits, one per finished step; the tree builds and the tests pass
  at every commit. Tick the step here in the same commit and rewrite "Next action".
- Nothing is pushed. No personal data in commits: no home paths, names, hosts, addresses, tokens.
- Tests are headless only and never talk to a running desktop: set `WAYLAND_DISPLAY` to the test
  compositor before any `wirbel` call.
- Checks per step: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.

## Next action

Phase 4, step 4.1.

## Phases

### 0 Docs
- [x] 0.1 RESEARCH.md, DESIGN.md, PROGRESS.md

### 1 Shadow core
- [x] 1.1 `--shadow NAME [WxH]` start mode: headless, injection on, no config, no binds, no
      borders or gaps, named control socket
- [x] 1.2 single layout in a shadow: first window fills the output, later ones float centred
- [x] 1.3 manager in the main compositor: registry, start, supervision, cleanup at exit
- [x] 1.4 requests `vwm open|list|info|close|resize|where` on the control socket
- [x] 1.5 command line `wirbel vwm open|list|info|close|resize` and the `invisible` aliases
- [x] 1.6 private D-Bus session bus per shadow, `--share-bus`, `--env`
- [x] 1.7 program recipes (Firefox, Chromium family) with profiles in the state folder
- [x] 1.8 integration tests `tests/vwm.rs`

### 2 Control
- [x] 2.1 absolute pointer requests: move, click, drag, scroll
- [x] 2.2 `capture` request writing a PNG
- [x] 2.3 clipboard get and set
- [x] 2.4 command line `vwm type|key|click|move|drag|scroll|shot|clip`
- [x] 2.5 tests

### 3 Events and waiting
- [x] 3.1 shadow events: damage, client exit, cursor, clipboard
- [x] 3.2 wait engine with deadlines; conditions ready, window, title, closed, exited, stable,
      changed, pixel, image, file
- [x] 3.3 command line `vwm wait|events`
- [x] 3.4 tests

### 4 Macros
- [ ] 4.1 lexer and parser with line numbers
- [ ] 4.2 interpreter: values, variables, expressions, control flow, functions, try
- [ ] 4.3 vwm steps, `on` handlers, timeouts
- [ ] 4.4 `wirbel macro run|check|list`, bind action `macro`
- [ ] 4.5 MACROS.md, examples, tests

### 5 Text I
- [ ] 5.1 clipboard trick
- [ ] 5.2 OCR through `tesseract`, positions, `wait text`

### 6 Text II
- [ ] 6.1 AT-SPI: tree, text, press, set
- [ ] 6.2 AT-SPI signals, `loaded`

### 7 Adoption
- [ ] 7.1 parked list, hidden from taskbar, frame callbacks
- [ ] 7.2 offscreen capture
- [ ] 7.3 `adopt`, `release`, focus borrowing behind `vwm_adopt_input`

### 8 Audio
- [ ] 8.1 null sink per instance, `record`, `listen`, `level`, sound and silence events

### 9 Video
- [ ] 9.1 `record` through `ffmpeg`, frame export

### 10 Browser protocol
- [ ] 10.1 `vwm web`, `loaded` and `text` through it

### 11 Viewer and finish
- [ ] 11.1 `vwm show|hide`
- [ ] 11.2 limits, memory column, README, GUIDE.md

## Notes and known problems

- Tests of shadows (`tests/vwm.rs`) start the real binary headless and need a GPU render node.
  The harness compositor plays the desktop; `Manager::binary` and `Manager::state_root` point it
  at the test binary and a scratch folder.
- `vwm open` answers once the shadow takes requests, not once the program has a window. Until
  `vwm wait NAME window` exists (phase 3), scripts sleep or poll `vwm info`.
- A command that cannot be started still gives a shadow, which ends after its 3 s of grace; the
  reason is in its `shadow.log`.
- The browser recipes (own profile for Firefox and the Chromium family) are covered by unit
  tests only; try a real browser in the smoke script of phase 2.
- `inject type` reaches the four levels of a key (plain, Shift, AltGr, both). Text the layout
  has no key for goes through `vwm paste` (clipboard and CTRL+v).
- Pictures and clipboard text travel as files named in the request, not over the socket: a
  request line is at most 4096 bytes. Their answers come late, through `Wirbel::reply_sender`
  or after the next frame.
- `shadow shot` works in the headless backend only (`vwm::shot::take` is called from its
  render). Parked windows (phase 7) need the same from the other backends.
- The clipboard type of the compositor is `vwm::clip::Held`: `None` stands for the X server,
  `Some` is text set from outside.
- Waiting polls instead of listening: `shadow status` answers damage count, quiet time, window
  count, program state, cursor, clipboard count and title in one line, and `vwm::wait` asks it
  every 40 ms. Pictures are taken only when the damage count moved. That replaced the pushed
  events and inotify of the design: same behaviour, far less code. `vwm events` prints the
  differences between two looks.
- A moving pointer and a blinking text cursor are damage too, so `stable` needs a time shorter
  than the blink of a terminal, and `changed` without a region fires on them.
- Still to come as conditions: `text GLOB` (phase 5), `loaded` (phase 6 and 10), `sound` and
  `silence` (phase 8). `Condition::parse` and `check` in `src/vwm/wait.rs` are the place.
- Try by hand without touching a running desktop: start `wirbel --headless --log FILE`, read
  the socket name from the log, set `WAYLAND_DISPLAY` to it, then `wirbel vwm ...`.
