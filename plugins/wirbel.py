"""Kleine Hilfe für wirbel-Plugins in Python, ohne Zusatzpakete.

    import wirbel
    w = wirbel.Wirbel()
    w.ask("offset focused 10 0")       # Befehl schicken, Antwort lesen
    for event in w.events():           # ["open", "3", "kitty"], ["focus", "3"], ...
        ...

Das Protokoll steht in docs/PLUGINS.md im wirbel-Repo.
"""

import json
import os
import socket
import threading


def _connect():
    path = os.environ.get("WIRBEL_SOCKET")
    if not path:
        raise SystemExit("WIRBEL_SOCKET fehlt: das Plugin muss von wirbel gestartet werden")
    sock = socket.socket(socket.AF_UNIX)
    sock.connect(path)
    return sock, sock.makefile("r", encoding="utf-8", errors="replace")


class Wirbel:
    def __init__(self):
        self._sock, self._answers = _connect()
        # Animationen laufen oft in eigenen Threads, die Antworten dürfen sich nicht mischen.
        self._lock = threading.Lock()

    def ask(self, line):
        """Schickt einen Befehl und gibt die Antwort zurück, z. B. "ok" oder "error no window 7"."""
        with self._lock:
            self._sock.sendall(line.encode() + b"\n")
            answer = self._answers.readline()
        if not answer:
            raise SystemExit("wirbel ist weg")
        return answer.rstrip("\n")

    def windows(self):
        return json.loads(self.ask("windows"))

    def events(self, *requests):
        """Liefert die Ereignisse ohne das Wort "event", auf einer eigenen Verbindung.

        `requests` gehen vorher über dieselbe Verbindung, etwa "grab ctrl+alt btn_left": die
        Ereignisse eines Grabs kommen nur dort an. Eine Fehlerantwort beendet das Plugin.
        """
        sock, lines = _connect()
        for request in ("subscribe", *requests):
            sock.sendall(request.encode() + b"\n")
            answer = lines.readline()
            while answer.startswith("event "):
                answer = lines.readline()
            if not answer.startswith("ok"):
                raise SystemExit(f"{request}: {answer.strip() or 'wirbel ist weg'}")
        for line in lines:
            words = line.rstrip("\n").split(" ", 3)
            if words and words[0] == "event":
                yield words[1:]
