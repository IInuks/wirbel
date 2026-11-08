# Plugins

A plugin is a program that wirbel starts with the session. It talks to wirbel through the control
socket and can make windows shake, fade, glow in other colours, react to windows opening and
closing, and run any bind action, all without a change to wirbel itself. It can be written in any
language that can open a Unix socket: Python, a shell with `socat`, Rust, C.

Plugins cannot crash wirbel. A plugin that dies only takes its own effects with it.

## Managing plugins

```
wirbel plug add ~/wirbel-plugins/wackeln.py          # add it, switched on, and start it
wirbel plug add ~/wirbel-plugins/wackeln.py wackeln  # the same, with a name
wirbel plug define ~/wirbel-plugins/wackeln.py wackeln
wirbel plug wackeln false                            # switch it off, it stops
wirbel plug wackeln true                             # and on again
wirbel plug define wackeln                           # take the name away
wirbel plug remove wackeln                           # out of the config
wirbel plug list
wirbel plug create ~/plugins                         # make a folder ready for plugins
```

`create` makes the folder if it is not there and puts `wirbel.py` (a small Python helper, so
`import wirbel` works) and `beispiel.py` (an example to start from) into it. Files that are already
there stay as they are, and the config is not touched: switch the example on with `wirbel plug add`.

Wherever a plugin is meant, its path or its name works. `list` shows the state, the name if there
is one, the path, and whether it runs:

```
on   wackeln     /home/me/wirbel-plugins/wackeln.py  running
off  regenbogen  /home/me/wirbel-plugins/regenbogen.py
on   /home/me/wirbel-plugins/einblenden.py  running
```

All of this ends up in the config as lines like

```
plugin = /home/me/wirbel-plugins/wackeln.py, wackeln, true
plugin = ~/wirbel-plugins/einblenden.py, true
```

which can be written by hand as well. `wirbel plug` asks a running wirbel to reload, which starts
the plugins that are on and stops those that are off or gone. A reload also starts a plugin again
that ended by itself. Names have no spaces, commas or slashes.

A plugin is an executable file (`chmod +x`), or a folder with an executable file `run` in it. It
runs in its own folder, in a process group of its own that ends with the session. What it prints
goes to `~/.local/state/wirbel/plugins/NAME.log`, which starts anew with every start.

It finds these in its environment:

| Variable          | What                                                     |
|-------------------|----------------------------------------------------------|
| `WIRBEL_SOCKET`   | the control socket                                       |
| `WIRBEL_PLUGIN`   | its name, or its path if it has none                     |
| `WIRBEL_CONFIG`   | the config wirbel reads                                  |
| `WAYLAND_DISPLAY` | as for any program, so a plugin can open windows as well |

## The protocol

One request per line, one answer per line: `ok`, `error MESSAGE`, or data. A connection may send
the next request before it has read the last answer; answers come in order. A client that reads
nothing for a long time is dropped, so read the answers.

### Windows

`windows` answers with one line of JSON, every window wirbel manages:

```json
[{"id":3,"app_id":"kitty","title":"~","output":"DP-4","workspace":1,"focused":true,
  "floating":false,"visible":true,"x":14,"y":14,"width":972,"height":572}]
```

`id` stays the same for the life of a window and is never used again. Windows on hidden workspaces
have `"visible":false` and `null` for the place.

### Events

`subscribe` answers `ok`, and from then on the connection gets a line whenever something happens:

```
event open ID APP_ID         a window appeared, APP_ID is - if it has none
event title ID TITLE         a window has a new title, also right after open
event close ID               a window is gone
event focus ID|none          the focus moved
event workspace OUTPUT N     an output shows another workspace, N from 1 to 9
```

Use a connection of its own for events, so that they do not get mixed up with answers.

### Effects

```
offset WINDOW DX DY           draw the window DX, DY pixels away from its place
opacity WINDOW ALPHA          draw it see-through, 0 is invisible, 1 is normal
border WINDOW 0xRRGGBB[AA]    give its border a colour of its own
border WINDOW none            take the colour back
reset WINDOW|all              take back everything this connection did to a window, or to all
```

`WINDOW` is an id or `focused`. Effects are only looks: for the mouse and the layout the window
stays where it is. They last as long as the connection that set them, so a plugin that ends leaves
nothing behind, and effects of several connections add up: offsets add, opacities multiply, the
newest border colour wins. An animation sends one request per step, 60 a second is no trouble.

### Actions

`dispatch ACTION [ARGUMENT]` does what a bind with that action does, with the names of the config:

```
dispatch spawn kitty
dispatch workspace next
dispatch move_to_workspace 3
dispatch float
dispatch close
```

### Mouse and drawing

For gestures and the like, see `~/wirbel-plugins/gesten.py` for a whole one.

```
grab MODIFIERS BUTTON         take BUTTON pressed with exactly MODIFIERS, like `grab ctrl+alt btn_left`
ungrab MODIFIERS BUTTON       give it back
line X1 Y1 X2 Y2 WIDTH COLOR  draw a line in front of everything but the cursor, WIDTH 1 to 64
clear [MS [COLOR]]            take back all lines of this connection, fading out over MS in COLOR
window_at X Y                 the id of the window there, or none
focus WINDOW                  give it the focus, bringing up its workspace
close WINDOW                  ask it to close
```

A grabbed press starts a session that lasts until one of the modifiers goes up. During it the button
may go up and down as often as it likes, none of that reaches a client, and the connection that
grabbed gets these lines, positions global:

```
event grab press X Y          the button went down, also the first press that started the session
event grab motion X Y         the pointer moved
event grab release X Y        the button went up
event grab end X Y            a modifier went up, the session is over
event grab cancel             Escape, a lock or leaving the VT ended it, Escape stays with wirbel
```

They come on the connection that grabbed, subscribed or not, so grab on the connection you read
events from. Grabs and lines last as long as the connection.

### Everything else

The requests of `wirbel pos`, `wirbel move`, `wirbel click` and `wirbel ws` work too, see the top of
`src/ipc.rs`: `position`, `watch` (one line whenever the pointer moves), `move X Y`,
`move-rel DX DY`, `click`, `workspace N`, `tags`, `monitors` and their `watch-` forms.

## A plugin in a few lines

```python
#!/usr/bin/env python3
# Every new window starts half see-through for a second.
import os, socket, threading, time

def connect():
    sock = socket.socket(socket.AF_UNIX)
    sock.connect(os.environ["WIRBEL_SOCKET"])
    return sock, sock.makefile("r")

events, event_lines = connect()
events.sendall(b"subscribe\n")
event_lines.readline()                      # ok

control, answers = connect()
lock = threading.Lock()                     # the timers ask from other threads
def ask(line):
    with lock:
        control.sendall(line.encode() + b"\n")
        return answers.readline().strip()

for line in event_lines:
    words = line.split()
    if words[:2] == ["event", "open"]:
        ask(f"opacity {words[2]} 0.5")
        threading.Timer(1, ask, [f"reset {words[2]}"]).start()
```
