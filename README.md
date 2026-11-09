# wirbel

A small tiling Wayland compositor written in Rust with [Smithay](https://github.com/Smithay/smithay).

## Installation

```sh
git clone https://github.com/IInuks/wirbel.git
cd wirbel
./setup.sh --check   # optional: changes nothing, shows what is missing
./setup.sh
```

`setup.sh` installs the packages it needs, Rust if it is missing, builds wirbel, adds a **wirbel**
entry to your login screen and writes a configuration to `~/.config/wirbel/config`.
It works on **Arch**, **Debian 13+ / Ubuntu**, **Void** and distributions based on them. Run it
as your normal user; it asks for `sudo` when it needs it.

| Option | What it does |
| --- | --- |
| `--check` | only shows what would happen |
| `--yes` | does not ask before installing packages |
| `--layout de` | keyboard layout (default: the one your system uses) |
| `--edit-config` | opens the configuration in your editor before it is installed |
| `--config FILE` | installs your own configuration instead of the generated one |
| `--replace-config` | replaces an existing configuration (a backup is kept) |
| `--remove` | uninstalls wirbel, your configuration stays |

**Updating:** `git pull && ./setup.sh`

After installing, log out and pick **wirbel** as the session on your login screen.

## What is wirbel?

wirbel arranges windows automatically in a *dwindle* layout: each new window splits the
focused one in half, alternating between side by side and on top of each other. You can float,
maximize and fullscreen windows, use workspaces 1–9 on every monitor, and drag windows with the
mouse.

It does what a daily desktop needs:

- multiple monitors, high refresh rates, per-monitor mode and position
- layer shell, so bars, launchers and wallpaper tools (Waybar, Quickshell, awww, …) work
- XWayland for X11 programs and games (Steam included)
- screen sharing through xdg-desktop-portal-wlr (Discord, OBS, browsers)
- a plain text config with keybinds that is reloaded with one key
- command-line tools to read and move the mouse, switch workspaces and more
- **plugins**: your own programs that can animate, colour and control windows

## Getting started

Default keys (all of them can be changed):

| Key | Action |
| --- | --- |
| `Super + Enter` | open a terminal (kitty) |
| `Super + W` or `Alt + Q` | close the window |
| `Alt + Arrows` | move the focus |
| `Super + Tab` | next window |
| `Super + Shift + Arrows` | swap the window with its neighbour |
| `Ctrl + Alt + Arrows` | make the window bigger / smaller |
| `Alt + ß` / `Alt + \` | float the window |
| `Alt + A` | maximize |
| `Alt + F` | fullscreen |
| `Super + 1…9` | go to workspace 1–9 |
| `Super + Shift + 1…9` | move the window to workspace 1–9 |
| `Super + Left / Right` | previous / next workspace |
| `Alt + Shift + Left / Right` | focus the other monitor |
| `Super + Alt + Left / Right` | move the window to the other monitor |
| `Super + Left mouse` | move a window (drag) |
| `Super + Right mouse` | resize a window (drag) |
| `Super + R` | reload the config |
| `Super + M` | quit wirbel |
| `Ctrl + Alt + Shift + Backspace` | emergency stop, always works |

## Configuration

The config lives in `~/.config/wirbel/config`. Every option is explained in
[`src/default.conf`](src/default.conf). Change something, press `Super + R`, and it applies.
To check the file for mistakes without starting wirbel, run `wirbel check`.

```ini
# looks
gap_outer = 10
gap_inner = 5
border_width = 4
focus_color = 0xc9b890

# keyboard
keyboard_layout = de

# programs that start with wirbel
exec-once = waybar

# monitors (optional, otherwise: native resolution at the highest refresh rate)
monitorrule = name:DP-1,width:2560,height:1440,refresh:165,x:0,y:0

# keys: bind = MODIFIERS, KEY, ACTION, ARGUMENT
bind = SUPER, Return, spawn, kitty
bind = SUPER, b, spawn, firefox
bind = ALT, f, fullscreen

# mouse wheel while holding a modifier
axisbind = SUPER, UP, workspace, prev_used

# mouse buttons: move or resize
mousebind = SUPER, btn_left, move
```

As soon as your config contains one `bind` line, it replaces all the default binds, so copy the
ones you want to keep. If no `quit` bind is left, `Super + M` is added so you can always get out.

Available actions: `spawn`, `close`, `quit`, `restart`, `reload`, `focus`, `focus_next`,
`focus_prev`, `swap`, `split_ratio`, `float`, `fullscreen`, `maximize`, `workspace`,
`move_to_workspace`, `focus_output`, `move_to_output`. Their arguments are in
[`src/default.conf`](src/default.conf).

## Command line

These commands talk to the running wirbel, so you can use them in scripts and keybinds.

| Command | What it does |
| --- | --- |
| `wirbel pos` | prints the mouse position |
| `wirbel pos loop xy 1` | prints the position live, one line per change, 1 decimal |
| `wirbel pos --json` | the same as JSON |
| `wirbel move 500 300` | moves the mouse to x=500, y=300 |
| `wirbel move --rel 50 0 0.5` | moves it 50 px to the right over 0.5 seconds |
| `wirbel click [COUNT\|loop] [SECONDS]` | clicks, optionally several times with a pause |
| `wirbel distance [SECONDS\|loop]` | measures how far the mouse travels |
| `wirbel ws 3` / `next` / `prev` / `next_used` / `prev_used` | switches workspace |
| `wirbel restart` | restarts the whole session, like logging out and in |
| `wirbel check [CONFIG]` | checks the config and shows your GPUs and monitors |
| `wirbel plug …` | manages plugins, see below |

`wirbel mmsg` (or a link named `mmsg`) understands the most common commands of Mango's `mmsg`,
so bars made for Mango can show wirbel's workspaces.

## Plugins

A plugin is any program (Python, shell, Rust, …) that wirbel starts with the session. It talks to
wirbel through its socket and can, for example, make windows wobble, fade in, glow in rainbow
colours, react to windows opening, or draw mouse gestures on screen. A plugin cannot crash wirbel.

```sh
wirbel plug create ~/plugins             # new folder with a helper (wirbel.py) and an example
wirbel plug add ~/plugins/beispiel.py    # add it and start it
wirbel plug list                         # what is installed and running
wirbel plug beispiel false               # switch it off (true = on again)
wirbel plug remove beispiel              # remove it from the config
```

Several plugins can be given at once, for example `wirbel plug add ~/plugins/*.py`.
Logs of every plugin are in `~/.local/state/wirbel/plugins/`.

How to write your own: [`docs/PLUGINS.md`](docs/PLUGINS.md).

## Trying it without logging out

Started from inside another Wayland or X11 session, wirbel opens as a window, which is handy for
testing a config:

```sh
wirbel --config ~/test.conf -c kitty
```

On a TTY it takes over the screen directly. The log is written to
`~/.local/state/wirbel/wirbel.log`.

## When something goes wrong

- **Stuck?** `Ctrl + Alt + Shift + Backspace` always ends wirbel and gives you back the TTY / login.
- **Config error?** `wirbel check` shows the line. A broken line is skipped, wirbel still starts.
- **Look at the log:** `~/.local/state/wirbel/wirbel.log`
- **Plugin not running?** Look in `~/.local/state/wirbel/plugins/NAME.log` and make sure the file
  is executable (`chmod +x`) and starts with a `#!` line.

## License

[MIT](LICENSE)
