# Wirbel VWM: invisible windows, control commands and macros

## Context

wirbel is to start and drive programs whose windows never appear on the visible
desktop: type, click, read text, take screenshots, record sound and video, wait for events, and
script all of it with an easy macro system. It must be built into wirbel (Rust, no plugin), work
for any Linux program and for several instances at once, and must never change the visible
focus or workspaces.

Decisions taken before the work began:

| Topic | Decision |
|---|---|
| Core architecture | Shadow instances: hidden headless wirbel processes, supervised by the main wirbel |
| Already open windows | "Park with limits": parked invisibly in the main wirbel, restricted keyboard input |
| Macros | Own small line-based language (`.wm` files), every step also a `wirbel vwm` command |
| Reading text | All four: AT-SPI, clipboard trick, OCR, browser protocol |
| OCR engine | `tesseract` as an optional helper program at run time  |

## What the code base already gives us

- `src/backend/headless.rs`: renders on a GPU render node into a buffer nobody sees, one virtual
  output, 60 Hz clock that only ticks when something asked for a frame.
- `src/inject.rs`: `Inject::{Key,Tap,Chord,Type,Motion,Button,Wheel}` through the real input
  paths (`on_key`, `on_pointer_*` in `src/input.rs`), enabled by `--headless`/`--test-input`.
- `src/handlers/screencopy.rs`: `Capturer` draws the scene once more into its own texture and
  reads it back; the pattern for a built-in capture request.
- `src/ipc.rs`, `src/cli.rs`: line-based control socket `wirbel-<display>.sock`
  (`ipc::socket_path`), `Request` parsing, CLI helpers `connect`/`send_line`/`read_line`.
- `src/plugin_api.rs`: `windows` JSON, `subscribe` events (open, close, title, focus), stable
  `window_id`.
- `src/window_hook.rs`: `held_windows`, `frame_windows`, `refresh_hook_outputs`,
  `release_held_windows`: how windows live outside the workspaces and still get frame callbacks.
  The single hook slot may be taken by an extension, so parking gets its own list and reuses the helpers.
- `src/state.rs`: `client_command` (environment for children), `start` (process groups,
  `safety::end_at_exit`), `listen`, `ClientState`.
- `src/handlers/selection.rs`: clipboard and data-control state per compositor.
- `src/test_harness.rs` and `tests/headless.rs`: compositor on its own thread plus a Wayland test
  client; all new tests build on it.
- wirbel has almost no third-party crates. Every new one below is deliberate and feature-gated.

Verified limits: the pinned Smithay has no per-client filter for `wl_seat`/`wl_output` globals
(private seats in the main compositor would need a fork), and a running Wayland client cannot be
moved to another compositor.

## Options compared (short form; full text goes to `docs/vwm/RESEARCH.md`)

| Approach | Controls | Invisible | Wayland | Rust fit | Cost | Role in the design |
|---|---|---|---|---|---|---|
| Shadow wirbel instance per window | any Wayland/X11 program | fully, own focus/seat/clipboard | native | existing code | ~30-60 MB each | **core** |
| Parked windows in the main compositor, private seats | Wayland programs started for it | fully | native | needs Smithay fork, touches daily input code | low RAM, high risk | rejected; parking without private seats kept for adoption |
| Foreign headless compositor (cage, sway, weston, Xvfb) | any | fully | yes / X11 only for Xvfb | external tools only | medium | rejected (not integrated) |
| Virtual input devices (uinput, virtual-keyboard, libei) | whatever has the real focus | no, acts on the visible seat | yes | fine | tiny | rejected for hidden windows |
| Accessibility (AT-SPI over D-Bus) | GTK, Qt, Firefox, Chromium/Electron; not games | n/a (no pixels) | independent | `zbus` | low | text, UI actions, events |
| Clipboard trick inside the shadow | programs with select-all/copy | fully (private clipboard) | native | in-tree | none | universal text fallback |
| Screen analysis and OCR | everything that draws | n/a | independent | `tesseract` helper | 0.2-1 s per read | text with positions, image waits |
| Browser protocol (WebDriver BiDi, CDP) | Firefox, Chromium family | n/a | independent | WebSocket crate | low | exact web text and load events |
| Direct process control (pipes, PTY, signals, exit status, inotify) | CLI/TUI programs, files | fully | independent | std/libc | none | events: exited, file created |
| PipeWire null sink per instance | any program with sound | fully (silent unless routed) | independent | `pactl`/`pw-record` helpers | low | audio record, listen, sound events |

Possible in practice: everything in the "core" and helper rows above. Only theoretical or
unreliable: full independent keyboard for an adopted, already running window; moving a running
window into a shadow; feeding a hidden program a virtual microphone or camera (possible with
PipeWire, not planned); "page loaded" for programs with neither accessibility nor a protocol
(only the heuristic: title changed, picture stable, no busy cursor).

## Architecture

```
wirbel (visible desktop)
 ├─ vwm manager (src/vwm/manager.rs): registry name → shadow process / parked window
 │    ├─ shadow "browser1": `wirbel --shadow browser1 1280x800` → firefox
 │    ├─ shadow "browser2": ...
 │    └─ parked "mail": adopted window, kept out of workspaces and taskbar
 └─ control socket: `vwm ...` lifecycle requests

wirbel vwm … / wirbel macro … (CLI process)
 ├─ asks the main socket where a name lives, then talks to that shadow's own control socket
 ├─ wait engine: one poll loop over shadow events, inotify, D-Bus signals, audio levels
 └─ macro interpreter (runs here, never inside a compositor)
```

- **Shadow instance** = the same binary with a new internal `--shadow NAME [WxH]` mode: headless
  backend, injection on, no config file, no binds, no borders or gaps, no autostart, no plugins.
  Layout "single": the main window fills the output (output size = window size), further
  toplevels (dialogs) float centred on top. Own process group, ended with the main session
  (`safety::end_at_exit`), crash-isolated from the desktop.
- **Program environment**: `WAYLAND_DISPLAY` and `DISPLAY` of the shadow, accessibility switches
  (`QT_LINUX_ACCESSIBILITY_ALWAYS_ON`, `GNOME_ACCESSIBILITY`), later `PULSE_SINK`. By default a
  private D-Bus session bus per shadow, so single-instance programs do not hand the window to
  their visible copy (`--share-bus` opts out). Built-in recipes add isolation flags for known
  programs (Firefox `--new-instance --profile DIR`, Chromium family `--user-data-dir=DIR`),
  profiles under `~/.local/state/wirbel/vwm/NAME/`.
- **Parked windows** (adoption): new list `Wirbel::parked` next to `held_windows`; removed from
  workspace, space and foreign-toplevel list; frame callbacks and output enter kept so the
  program keeps drawing. Screenshots by offscreen render, text and actions by AT-SPI, pointer and
  keyboard only by "focus borrowing" (config `vwm_adopt_input = off|borrow`, default off, refused
  while the user holds keys or buttons, during grabs and while locked). `release` puts it back.
- **One target abstraction** in the CLI (`Target::Shadow(socket)` / `Target::Parked(id)`), so
  every command and macro step works on both, with clear errors where parking cannot do it.

## Command line (the `invisible …` forms stay valid as aliases)

```
wirbel vwm open [--name N] [--size WxH] [--share-bus] [--env K=V]... COMMAND...
wirbel vwm adopt WINDOW|focused [--name N]      wirbel vwm release NAME
wirbel vwm list [--json]                        wirbel vwm info NAME
wirbel vwm close NAME|all [--kill]              wirbel vwm resize NAME WxH
wirbel vwm type NAME TEXT                       wirbel vwm key NAME KEY|MODS+KEY
wirbel vwm click NAME X Y [left|right|middle] [COUNT]
wirbel vwm move NAME X Y [SECONDS]              wirbel vwm drag NAME X1 Y1 X2 Y2
wirbel vwm scroll NAME STEPS [X Y]
wirbel vwm shot NAME [FILE.png] [--region X,Y,W,H]
wirbel vwm clip NAME get|set TEXT
wirbel vwm text NAME [--via a11y|clipboard|ocr|web|auto] [--find GLOB] [--where]
wirbel vwm ui NAME tree | press LABEL | set LABEL TEXT
wirbel vwm wait NAME CONDITION [--timeout S]    wirbel vwm events NAME
wirbel vwm audio NAME record FILE [S] | listen on|off | level
wirbel vwm record NAME FILE [--fps N] [S]
wirbel vwm web NAME goto URL | text SELECTOR | click SELECTOR | eval JS
wirbel vwm show NAME | hide NAME
wirbel macro run FILE|NAME [ARGS]... | check FILE | list
```

`wirbel vwm invisible create|type|key|screenshot|close …` map to `open|type|key|shot|close`.
Exit codes: 0 ok, 1 error, 2 usage, 3 timeout, so shell scripts can use every command too.

## Events and waiting

Conditions: `ready`, `window [TITLE]`, `title GLOB`, `closed`, `exited`, `loaded`,
`stable [MS]`, `changed [REGION]`, `pixel X Y COLOR`, `image FILE [SIMILARITY]`, `text GLOB`,
`cursor NAME`, `clipboard`, `file PATH`, `sound`, `silence [MS]`.

- Pushed by the shadow over `subscribe`: window open/close/title, client exit, damage timestamps
  (so `stable` and `changed` cost no pixel reads), cursor shape, clipboard change.
- Pushed from outside: AT-SPI signals (document loaded, text changed, state changed), inotify for
  files, PCM level from the instance's audio monitor.
- Polled only after damage: `text` by OCR, `image`, `pixel`.
- `loaded` uses the best source present: browser protocol, then AT-SPI, then the heuristic; the
  answer says which one decided.
- Every wait has a deadline (default 30 s, exit code 3), and reports the last seen state on timeout.

## Macro language (`.wm`, interpreter in `src/macros/`)

```
param url = "https://example.com"
open browser1 = firefox
on browser1 text "*Cookies*" do
    key browser1 Escape
end
type browser1 "$url"
key browser1 Return
wait browser1 loaded timeout 20s
try
    let titel = text browser1 find "Example*"
else
    fail "text not found: $error"
end
print "Found: $titel"
close browser1
```

Variables (`let`, `set`), strings with `$name`, numbers, booleans, lists; `if/elif/else`,
`while`, `for … in`, `repeat N`; `func`/`return`; `use FILE` for reuse; `try/else` with `$error`,
`fail`, `exit`; `on TARGET CONDITION do … end` handlers that run during waits and between steps;
`timeout` per step and as a default; `print`, `ask`, `shell`, `read`/`write` files; parameters from
the command line; `--json` result output. Errors name file and line. Macros live in
`~/.config/wirbel/macros/`; a bind action `macro NAME` runs one. Written by hand, no new crate.

## New dependencies (all pure Rust, feature-gated, default on)

- `png`: screenshots and image comparison (always).
- `zbus` (blocking API): AT-SPI, feature `vwm-a11y`.
- `tungstenite`: browser protocol, feature `vwm-web`; JSON read by a small in-tree parser.
- Run-time helper programs, optional, with a clear message when missing: `tesseract` (installed
  separately), `pactl`/`pw-record`, `ffmpeg`, `dbus-daemon`.

## Phases

Every phase ends with: all tests green, clippy and fmt clean, one commit, `PROGRESS.md` updated.

0. **Docs and safety net**: branch `vwm`; `docs/vwm/RESEARCH.md` (each approach answered on the
   eight questions), `DESIGN.md`, `PROGRESS.md` with the resume protocol.
1. **Shadow core**: `--shadow` mode and single layout (`src/app.rs`, `src/backend/headless.rs`,
   new `src/vwm/shadow.rs`); manager and registry (`src/vwm/manager.rs`, field in `src/state.rs`);
   requests `vwm open|list|info|close|resize` (`src/ipc.rs`); CLI (`src/vwm/cli.rs`, hooked into
   `src/cli.rs` and the usage text); private bus and program recipes; supervision and cleanup.
2. **Control**: absolute pointer steps (`inject move/click/drag/scroll` in `src/inject.rs`),
   `capture` request with PNG (reusing `Capturer`), clipboard get/set; `vwm type|key|click|move|
   drag|scroll|shot|clip`.
3. **Events and waiting**: new shadow events in `src/plugin_api.rs`; wait engine
   `src/vwm/wait.rs` (poll loop, inotify via libc, pixel/changed/image); `vwm wait|events`.
4. **Macros**: `src/macros/{lexer,parser,interp}.rs`, `wirbel macro run|check|list`, bind action,
   `docs/vwm/MACROS.md`, examples in `examples/macros/`.
5. **Text I**: clipboard trick and OCR (`src/vwm/text.rs`), `text`, `--where` positions,
   `wait text`.
6. **Text II**: AT-SPI (`src/vwm/a11y.rs`): tree, text, press, set, signals, `loaded`.
7. **Adoption**: `parked` list, offscreen capture, taskbar hiding, focus borrowing, `adopt`/
   `release` (`src/vwm/park.rs`, `src/handlers/foreign_toplevel.rs`, `src/render.rs`).
8. **Audio**: null sink per instance, `record`, `listen`, `level`, `sound`/`silence` events.
9. **Video**: `record` through an `ffmpeg` pipe fed on damage, PNG frame export.
10. **Browser protocol**: `src/vwm/web.rs`, `vwm web …`, `loaded` and `text` through it.
11. **Viewer and finish**: `vwm show|hide` (a normal window showing the shadow live, input passed
    on), limits and RAM column in `vwm list`, README section, `docs/vwm/GUIDE.md`.

## Verification

- Unit tests: request parsing, registry, macro lexer/parser/interpreter, PNG round trip, glob and
  image matching, wait deadlines.
- Headless integration tests (`tests/vwm.rs`, test harness): a headless main wirbel opens two
  shadows with the harness test client; typing reaches only the named one; the main compositor's
  focus, workspaces and `windows` list do not change; `shot` returns the client's colour; `close`
  and a main exit leave no process or socket behind; a parked window disappears from the
  foreign-toplevel list and returns on `release`.
- Macro tests: example macros run against the test client, including timeout, `try/else` and an
  `on` handler.
- Real-program smoke script `scripts/vwm-smoke.sh` (nested headless main, never the live one):
  `foot` and Firefox opened invisibly, a URL typed, `wait loaded`, `text`, `shot` checked by eye
  , audio level from a test tone.
- Per phase: `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check`.
- Final check on a live session after installing: `wirbel vwm open`, nothing
  visible, `wirbel vwm list` shows it, focus stays put.
