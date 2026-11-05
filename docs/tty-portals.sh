#!/bin/sh
# Starts the screen sharing portals for a test in wirbel. Runs as a child of wirbel, which has to
# run under `dbus-run-session --config-file=docs/tty-dbus.conf`, see there why. Without Mango's
# start-portals on purpose: that one kills by pattern and would hit the portals of the running Mango
# session.
#
# xdg-desktop-portal-wlr ends with the Wayland connection. The portal itself may outlive the bus,
# so it goes by its PID right after that.
# $1: a configuration for xdg-desktop-portal-wlr, without it the output is picked with slurp.
#
# A bus that can start services must never see these portals: xdg-desktop-portal asks for
# org.freedesktop.portal.Documents, the bus starts a second xdg-document-portal, and that takes the
# mount in $XDG_RUNTIME_DIR/doc from the one of the running session. Bedrock shows the service files of
# Arch to every stratum, so that holds for each bus but docs/tty-dbus.conf.
if ! names=$(strat arch dbus-send --session --print-reply --dest=org.freedesktop.DBus \
    /org/freedesktop/DBus org.freedesktop.DBus.ListActivatableNames) ||
    printf %s "$names" | grep -q org.freedesktop.portal; then
    echo "tty-portals: not on a private bus from docs/tty-dbus.conf, not starting" >&2
    exit 1
fi
export XDG_CURRENT_DESKTOP=wlroots
strat arch /usr/lib/xdg-desktop-portal-wlr ${1:+-c "$1"} &
wlr=$!
sleep 0.5
strat arch /usr/lib/xdg-desktop-portal &
portal=$!
wait "$wlr"
kill "$portal"
