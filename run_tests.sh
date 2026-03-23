#!/bin/bash
set -e

# OMT Rust Port Build & Test Script

echo "=== OMT Rust Environment Setup & Test ==="

# 1. Check for Rust Toolchain
if ! command -v cargo &> /dev/null; then
    echo "Rust not found. Installing rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
    source "$HOME/.cargo/env"
else
    echo "Rust toolchain is already installed."
fi

# 2. Check for System Dependencies (Linux specific)
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    echo "Checking Linux system dependencies..."
    
    MISSING_DEPS=""
    
    # Check for clang (needed for bindgen)
    if ! command -v clang &> /dev/null; then
        MISSING_DEPS="$MISSING_DEPS clang"
    fi

    # Check for pkg-config
    if ! command -v pkg-config &> /dev/null; then
        MISSING_DEPS="$MISSING_DEPS pkg-config"
    fi

    # Check for alsa headers
    if ! pkg-config --exists alsa 2>/dev/null; then
        MISSING_DEPS="$MISSING_DEPS libasound2-dev"
    fi

    # Check for v4l headers
    if ! pkg-config --exists libv4l2 2>/dev/null; then
        MISSING_DEPS="$MISSING_DEPS libv4l-dev"
    fi

    if [ ! -z "$MISSING_DEPS" ]; then
        echo "Missing dependencies:$MISSING_DEPS. Installing..."
        sudo apt-get update
        sudo apt-get install -y $MISSING_DEPS
    else
        echo "All system dependencies (clang/alsa/v4l/pkg-config) are present."
    fi
fi

# 3. Workspace Build & Check
echo "--------------------------------------------------"
echo "Running cargo check for the entire workspace..."
cargo check --workspace

# 4. Run unit tests
echo "--------------------------------------------------"
echo "Testing libomtnet..."
cargo test -p libomtnet

echo "--------------------------------------------------"
echo "Testing omtcapture settings..."
cargo test -p omtcapture

echo "--------------------------------------------------"
echo "Build and Test completed successfully."
