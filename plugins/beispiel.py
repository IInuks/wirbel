#!/usr/bin/env python3
"""Vorlage für ein eigenes wirbel-Plugin. Schreibt jedes Ereignis ins Log.

Einschalten:  wirbel plug add PFAD/beispiel.py beispiel
Log:          ~/.local/state/wirbel/plugins/beispiel.log
Protokoll:    docs/PLUGINS.md im wirbel-Repo
"""

import wirbel

w = wirbel.Wirbel()
print("Fenster:", [f["app_id"] for f in w.windows()], flush=True)

for event in w.events():
    # z. B. ["open", "3", "kitty"], ["focus", "3"], ["close", "3"]
    print("Ereignis:", *event, flush=True)
