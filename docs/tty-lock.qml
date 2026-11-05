// A minimal locker for docs/TTY-TEST.md, there is no other on this system. Enter unlocks.
import Quickshell
import Quickshell.Wayland
import QtQuick

ShellRoot {
    WlSessionLock {
        id: lock
        locked: true

        WlSessionLockSurface {
            color: "#203040"

            Text {
                anchors.centerIn: parent
                text: "Gesperrt. Enter entsperrt."
                color: "white"
                font.pixelSize: 32
            }

            Item {
                anchors.fill: parent
                focus: true
                Keys.onReturnPressed: {
                    lock.locked = false;
                    Qt.quit();
                }
            }
        }
    }
}
