#!/bin/bash
set -e

# OMT Rust Port Build & Test Script

echo "=== OMT Rust Environment Setup & Test ==="

# 1. Check for Rust Toolchain
if ! command -v cargo &> /dev/null; then
    echo "Rust not found. Installing rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
else
    echo "Rust toolchain is already installed."
fi

# 2. Check for System Dependencies (Linux specific)
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    echo "Checking Linux system dependencies..."
    
    # Check for clang (needed for bindgen)
    if ! command -v clang &> /dev/null; then
        echo "clang not found. Installing..."
        sudo apt-get update && sudo apt-get install -y clang libasound2-dev libv4l-dev pkg-config
    else
        echo "Environment looks good (clang/alsa/v4l found)."
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
