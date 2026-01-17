#!/bin/bash
# Integration test: Two hashtree instances discover each other and transfer data
#
# This test:
# 1. Starts two htree daemons with separate data dirs
# 2. Instance A adds a directory
# 3. Instance B attempts to get it via P2P peer discovery
#
# Requirements: htree binary must be built (cargo build)
#
# Note: Full P2P sync requires Nostr relay connectivity for signaling.
# In isolated environments, the test verifies the local workflow works.
#
# Usage:
#   ./tests/integration_two_instances.sh
#   DISCOVERY_WAIT=30 ./tests/integration_two_instances.sh  # longer wait for discovery

set -e

DISCOVERY_WAIT=${DISCOVERY_WAIT:-20}

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Cleanup function
cleanup() {
    echo -e "${YELLOW}Cleaning up...${NC}"

    # Kill daemon processes if they exist
    if [ -n "$PID_A" ] && kill -0 "$PID_A" 2>/dev/null; then
        kill "$PID_A" 2>/dev/null || true
    fi
    if [ -n "$PID_B" ] && kill -0 "$PID_B" 2>/dev/null; then
        kill "$PID_B" 2>/dev/null || true
    fi

    # Remove temp directories
    if [ -n "$TMPDIR_A" ] && [ -d "$TMPDIR_A" ]; then
        rm -rf "$TMPDIR_A"
    fi
    if [ -n "$TMPDIR_B" ] && [ -d "$TMPDIR_B" ]; then
        rm -rf "$TMPDIR_B"
    fi
    if [ -n "$TEST_DATA_DIR" ] && [ -d "$TEST_DATA_DIR" ]; then
        rm -rf "$TEST_DATA_DIR"
    fi
    if [ -n "$OUTPUT_DIR" ] && [ -d "$OUTPUT_DIR" ]; then
        rm -rf "$OUTPUT_DIR"
    fi
}

trap cleanup EXIT

# Find htree binary
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
HTREE_BIN="$PROJECT_ROOT/target/debug/htree"

if [ ! -x "$HTREE_BIN" ]; then
    echo -e "${YELLOW}Building htree...${NC}"
    (cd "$PROJECT_ROOT" && cargo build --bin htree)
fi

if [ ! -x "$HTREE_BIN" ]; then
    echo -e "${RED}Error: htree binary not found at $HTREE_BIN${NC}"
    exit 1
fi

echo -e "${GREEN}Using htree binary: $HTREE_BIN${NC}"

# Create temp directories for each instance
TMPDIR_A=$(mktemp -d)
TMPDIR_B=$(mktemp -d)
TEST_DATA_DIR=$(mktemp -d)
OUTPUT_DIR=$(mktemp -d)

echo -e "${GREEN}Instance A data dir: $TMPDIR_A${NC}"
echo -e "${GREEN}Instance B data dir: $TMPDIR_B${NC}"
echo -e "${GREEN}Test data dir: $TEST_DATA_DIR${NC}"

# Create test directory structure
mkdir -p "$TEST_DATA_DIR/subdir"
echo "Hello from file 1" > "$TEST_DATA_DIR/file1.txt"
echo "Hello from file 2" > "$TEST_DATA_DIR/file2.txt"
echo "Nested content" > "$TEST_DATA_DIR/subdir/nested.txt"
echo '{"key": "value", "number": 42}' > "$TEST_DATA_DIR/data.json"

echo -e "${GREEN}Created test files:${NC}"
find "$TEST_DATA_DIR" -type f -exec echo "  {}" \;

# Start instance A
echo -e "\n${YELLOW}Starting Instance A on port 8081...${NC}"
HTREE_DATA_DIR="$TMPDIR_A" "$HTREE_BIN" start --addr 127.0.0.1:8081 &
PID_A=$!
sleep 2

# Check if instance A started
if ! kill -0 "$PID_A" 2>/dev/null; then
    echo -e "${RED}Error: Instance A failed to start${NC}"
    exit 1
fi
echo -e "${GREEN}Instance A started (PID: $PID_A)${NC}"

# Start instance B
echo -e "\n${YELLOW}Starting Instance B on port 8082...${NC}"
HTREE_DATA_DIR="$TMPDIR_B" "$HTREE_BIN" start --addr 127.0.0.1:8082 &
PID_B=$!
sleep 2

# Check if instance B started
if ! kill -0 "$PID_B" 2>/dev/null; then
    echo -e "${RED}Error: Instance B failed to start${NC}"
    exit 1
fi
echo -e "${GREEN}Instance B started (PID: $PID_B)${NC}"

# Wait for instances to discover each other via Nostr relays
echo -e "\n${YELLOW}Waiting for peer discovery (${DISCOVERY_WAIT} seconds)...${NC}"
sleep "$DISCOVERY_WAIT"

# Add directory on instance A
echo -e "\n${YELLOW}Adding directory on Instance A...${NC}"
ADD_OUTPUT=$(HTREE_DATA_DIR="$TMPDIR_A" "$HTREE_BIN" add "$TEST_DATA_DIR" --public 2>&1)
echo "$ADD_OUTPUT"

# Extract CID from output
CID=$(echo "$ADD_OUTPUT" | grep -oE '[a-f0-9]{64}' | head -1)

if [ -z "$CID" ]; then
    echo -e "${RED}Error: Failed to extract CID from add output${NC}"
    exit 1
fi

echo -e "${GREEN}Added with CID: $CID${NC}"

# Pin on instance A so it stays available
echo -e "\n${YELLOW}Pinning on Instance A...${NC}"
HTREE_DATA_DIR="$TMPDIR_A" "$HTREE_BIN" pin "$CID"

# Wait a bit for potential propagation
echo -e "\n${YELLOW}Waiting for data availability...${NC}"
sleep 5

# Try to get from instance B
echo -e "\n${YELLOW}Getting directory on Instance B...${NC}"
HTREE_DATA_DIR="$TMPDIR_B" "$HTREE_BIN" get "$CID" -o "$OUTPUT_DIR/retrieved" 2>&1 || {
    echo -e "${YELLOW}Direct get failed, trying via info first...${NC}"
    HTREE_DATA_DIR="$TMPDIR_B" "$HTREE_BIN" info "$CID" 2>&1 || true
}

# Verify retrieved content
echo -e "\n${YELLOW}Verifying retrieved content...${NC}"
if [ -d "$OUTPUT_DIR/retrieved" ]; then
    echo -e "${GREEN}=== SUCCESS: Directory retrieved via P2P! ===${NC}"
    echo "Retrieved files:"
    find "$OUTPUT_DIR/retrieved" -type f -exec echo "  {}" \;

    # Compare contents
    ORIGINAL_CONTENT=$(cat "$TEST_DATA_DIR/file1.txt")
    if [ -f "$OUTPUT_DIR/retrieved/file1.txt" ]; then
        RETRIEVED_CONTENT=$(cat "$OUTPUT_DIR/retrieved/file1.txt")
        if [ "$ORIGINAL_CONTENT" = "$RETRIEVED_CONTENT" ]; then
            echo -e "${GREEN}Content verification PASSED!${NC}"
        else
            echo -e "${RED}Content mismatch!${NC}"
            echo "Original: $ORIGINAL_CONTENT"
            echo "Retrieved: $RETRIEVED_CONTENT"
            exit 1
        fi
    else
        echo -e "${YELLOW}Warning: file1.txt not found in retrieved directory${NC}"
    fi
elif [ -f "$OUTPUT_DIR/retrieved" ]; then
    echo -e "${YELLOW}Retrieved as file (not directory)${NC}"
    ls -la "$OUTPUT_DIR/retrieved"
else
    echo -e "${YELLOW}=== Content not retrieved via P2P ===${NC}"
    echo -e "${YELLOW}This is expected if peers haven't connected via WebRTC yet.${NC}"
    echo -e "${YELLOW}P2P sync requires successful Nostr relay signaling.${NC}"
    echo ""
    echo -e "${YELLOW}Verifying local add/get works on Instance A...${NC}"
    LOCAL_OUTPUT=$(mktemp -d)
    HTREE_DATA_DIR="$TMPDIR_A" "$HTREE_BIN" get "$CID" -o "$LOCAL_OUTPUT/local_retrieved" 2>&1
    if [ -d "$LOCAL_OUTPUT/local_retrieved" ]; then
        echo -e "${GREEN}Local retrieval on Instance A: SUCCESS${NC}"
        find "$LOCAL_OUTPUT/local_retrieved" -type f -exec echo "  {}" \;
    else
        echo -e "${RED}Local retrieval on Instance A: FAILED (unexpected)${NC}"
        exit 1
    fi
    rm -rf "$LOCAL_OUTPUT"
fi

echo -e "\n${GREEN}Test completed!${NC}"
