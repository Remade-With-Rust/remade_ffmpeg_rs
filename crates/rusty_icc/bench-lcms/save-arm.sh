#!/usr/bin/env bash
# Keeps the current WSL icc_ir build as a named arm for wall.sh.
# Usage: save-arm.sh NAME
set -euo pipefail
mkdir -p /root/iccbench/arms
cp /root/iccbench/rusty_icc/target/release/examples/icc_ir "/root/iccbench/arms/$1"
echo "saved arm $1 $(sha256sum "/root/iccbench/arms/$1" | cut -c1-12)"
