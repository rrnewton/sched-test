#!/bin/bash
# Setup D-Bus session bus in virtme-ng environment

# Create runtime directory
export XDG_RUNTIME_DIR=/run/user/$(id -u)
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

# Create D-Bus directories
mkdir -p /run/dbus
mkdir -p /var/run/dbus

# Start system D-Bus if not running
if [ ! -S /run/dbus/system_bus_socket ]; then
    echo "Starting system D-Bus..."
    mkdir -p /run/dbus
    dbus-broker-launch --scope system --address=unix:path=/run/dbus/system_bus_socket &
    sleep 1
fi

# Start user D-Bus session
echo "Starting user D-Bus session..."
eval $(dbus-broker-launch --scope user 2>&1 | grep -E 'DBUS_SESSION_BUS_ADDRESS|DBUS_SESSION_BUS_PID')

# Export for current session
export DBUS_SESSION_BUS_ADDRESS
export DBUS_SESSION_BUS_PID

echo "D-Bus session started:"
echo "  DBUS_SESSION_BUS_ADDRESS=$DBUS_SESSION_BUS_ADDRESS"
echo "  DBUS_SESSION_BUS_PID=$DBUS_SESSION_BUS_PID"

# Test D-Bus
if [ -n "$DBUS_SESSION_BUS_ADDRESS" ]; then
    echo "D-Bus session is ready"
else
    echo "Failed to start D-Bus session"
    exit 1
fi
