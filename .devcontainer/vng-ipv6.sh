#!/bin/bash
# vng-ipv6.sh - Launch vng with working IPv6 networking on IPv6-only hosts
#
# This script works around the issue where vng's default --network user
# option uses QEMU's slirp networking which only supports IPv4, causing
# networking to fail on IPv6-only devservers.
#
# Solution: Use passt (Package Slirp Replacement Thingy) which supports IPv6

set -e

# Configuration
PASST_SOCKET="/tmp/vng-passt/passt.socket"
PASST_DIR="/tmp/vng-passt"
VM_CID="2222"  # vsock CID for SSH

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Parse command line arguments
CPUS="${CPUS:-4}"
MEMORY="${MEMORY:-4G}"
RWDIR="${RWDIR:-/usr/workspace=`pwd`}"
EXTRA_ARGS=""

print_usage() {
    cat << EOF
Usage: $0 [options]

Launch vng with working IPv6 networking on IPv6-only hosts.

Options:
    -c, --cpus N        Number of CPUs (default: 4)
    -m, --memory SIZE   Memory size (default: 4G)
    -d, --rwdir PATH    Read-write directory mapping (default: /usr/workspace=\$(pwd))
    -h, --help          Show this help message

Additional arguments are passed to vng.

Environment variables:
    CPUS     - Number of CPUs (same as -c)
    MEMORY   - Memory size (same as -m)
    RWDIR    - Read-write directory (same as -d)

Examples:
    $0                          # Launch with defaults
    $0 -c 8 -m 8G              # 8 CPUs, 8GB RAM
    $0 -- --debug              # Pass --debug to vng
EOF
}

# Parse arguments
while [[ $# -gt 0 ]]; do
    case $1 in
        -c|--cpus)
            CPUS="$2"
            shift 2
            ;;
        -m|--memory)
            MEMORY="$2"
            shift 2
            ;;
        -d|--rwdir)
            RWDIR="$2"
            shift 2
            ;;
        -h|--help)
            print_usage
            exit 0
            ;;
        --)
            shift
            EXTRA_ARGS="$@"
            break
            ;;
        *)
            EXTRA_ARGS="$EXTRA_ARGS $1"
            shift
            ;;
    esac
done

echo -e "${GREEN}=== vng IPv6 Networking Wrapper ===${NC}"
echo "Configuration:"
echo "  CPUs: $CPUS"
echo "  Memory: $MEMORY"
echo "  RW Dir: $RWDIR"
[[ -n "$EXTRA_ARGS" ]] && echo "  Extra args: $EXTRA_ARGS"
echo

# Check if we're on an IPv6-only host
if ip -4 route show default 2>/dev/null | grep -q default; then
    echo -e "${YELLOW}Warning: This host has IPv4 connectivity. You may not need this wrapper.${NC}"
    echo -e "${YELLOW}You can probably use: vng -r --network user${NC}"
    echo
fi

# Step 1: Start passt if not running
echo -e "${GREEN}[1/4] Checking passt...${NC}"
if pgrep -f "passt.*$PASST_SOCKET" > /dev/null; then
    echo "  passt is already running"
else
    echo "  Starting passt..."
    mkdir -p "$PASST_DIR"
    passt --foreground --socket "$PASST_SOCKET" &
    PASST_PID=$!

    # Wait for socket to be created
    for i in {1..10}; do
        if [[ -S "$PASST_SOCKET" ]]; then
            echo -e "  ${GREEN}✓${NC} passt started (PID: $PASST_PID)"
            break
        fi
        sleep 0.5
    done

    if [[ ! -S "$PASST_SOCKET" ]]; then
        echo -e "  ${RED}✗${NC} Failed to start passt"
        exit 1
    fi
fi

# Step 2: Launch vng
echo -e "${GREEN}[2/4] Launching VM...${NC}"
echo "  Command: vng -r --cpus $CPUS --memory $MEMORY --ssh --rwdir $RWDIR $EXTRA_ARGS"

# Create a temporary file to capture VM output
VM_OUTPUT=$(mktemp /tmp/vng-ipv6-XXXXXX.log)
trap "rm -f $VM_OUTPUT" EXIT

# Launch vng in background and capture output
vng -r --cpus "$CPUS" --memory "$MEMORY" --ssh --rwdir "$RWDIR" \
  --qemu-opts="-device virtio-net-pci,netdev=passt0 -netdev stream,id=passt0,server=off,addr.type=unix,addr.path=$PASST_SOCKET" \
  $EXTRA_ARGS 2>&1 | tee "$VM_OUTPUT" &

VNG_PID=$!

# Wait for VM to boot (look for the bash prompt)
echo "  Waiting for VM to boot..."
for i in {1..30}; do
    if grep -q "bash-5.1#" "$VM_OUTPUT" 2>/dev/null; then
        echo -e "  ${GREEN}✓${NC} VM booted successfully"
        break
    fi
    sleep 1
done

if ! grep -q "bash-5.1#" "$VM_OUTPUT" 2>/dev/null; then
    echo -e "  ${YELLOW}⚠${NC} VM may not have booted properly, continuing anyway..."
fi

# Step 3: Configure networking inside VM
echo -e "${GREEN}[3/4] Configuring network inside VM...${NC}"

# Try to configure network via SSH
for attempt in {1..5}; do
    if ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         -o ProxyCommand="socat - VSOCK-CONNECT:$VM_CID:22" \
         -o ConnectTimeout=5 \
         root@localhost "ip link set eth0 up && dhclient eth0 2>/dev/null" 2>/dev/null; then
        echo -e "  ${GREEN}✓${NC} Network configured"
        break
    else
        echo "  Attempt $attempt/5 failed, retrying..."
        sleep 2
    fi
done

# Step 4: Test connectivity
echo -e "${GREEN}[4/4] Testing connectivity...${NC}"
if ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
     -o ProxyCommand="socat - VSOCK-CONNECT:$VM_CID:22" \
     -o ConnectTimeout=5 \
     root@localhost "ping -6 -c 1 -W 2 internalfb.com >/dev/null 2>&1" 2>/dev/null; then
    echo -e "  ${GREEN}✓${NC} IPv6 connectivity working!"
else
    echo -e "  ${YELLOW}⚠${NC} Could not verify connectivity"
fi

echo
echo -e "${GREEN}=== VM Ready ===${NC}"
echo "The VM is now running with working IPv6 networking."
echo
echo "To connect via SSH:"
echo -e "  ${YELLOW}ssh -o ProxyCommand=\"socat - VSOCK-CONNECT:$VM_CID:22\" root@localhost${NC}"
echo
echo "To run a command:"
echo -e "  ${YELLOW}ssh -o ProxyCommand=\"socat - VSOCK-CONNECT:$VM_CID:22\" root@localhost \"your-command\"${NC}"
echo
echo "VM console is attached to this terminal. Press Ctrl+D to exit."
echo

# Attach to the VM console
wait $VNG_PID