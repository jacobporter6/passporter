#!/usr/bin/env bash
# Waits, at boot, for the NVIDIA driver's device nodes before whisper-server
# starts. Run by whisper-server.service as ExecStartPre; build.sh installs it
# to ~/.local/libexec/passporter/whisper-await-gpu.
#
# Why: the user manager can start whisper-server a few seconds before the
# nodes exist (the kernel module is loaded, /dev/nvidiactl is not there yet).
# CUDA then reports "no CUDA-capable device", whisper.cpp falls back to the
# CPU for the life of the process, and dictation runs at about real time
# instead of 25x faster.
#
# Never fails the unit: with no NVIDIA GPU in the machine it returns at once,
# and if the nodes have not appeared after WHISPER_GPU_WAIT_SECS (default 60,
# well past the few seconds a normal boot takes) it logs a warning and lets
# whisper-server start on the CPU.
set -uo pipefail

wait_secs="${WHISPER_GPU_WAIT_SECS:-60}"
if ! [[ "$wait_secs" =~ ^[0-9]+$ ]]; then
    echo "WHISPER_GPU_WAIT_SECS must be a whole number of seconds, not ${wait_secs}; using 60" >&2
    wait_secs=60
fi

# An NVIDIA (vendor 0x10de) display controller (PCI class 0x03xxxx). The PCI
# bus is enumerated long before the driver loads, so this is safe this early.
has_nvidia_gpu() {
    local dev vendor class
    for dev in /sys/bus/pci/devices/*; do
        read -r vendor 2>/dev/null <"${dev}/vendor" || continue
        read -r class 2>/dev/null <"${dev}/class" || continue
        [[ "$vendor" == 0x10de && "$class" == 0x03* ]] && return 0
    done
    return 1
}

# What CUDA opens: the control node, a device node and the unified-memory node.
nodes_ready() {
    local gpu
    [[ -e /dev/nvidiactl && -e /dev/nvidia-uvm ]] || return 1
    for gpu in /dev/nvidia[0-9]*; do
        [[ -e "$gpu" ]] && return 0
    done
    return 1
}

if ! has_nvidia_gpu; then
    exit 0
fi

start=$SECONDS
until nodes_ready; do
    if (( SECONDS - start >= wait_secs )); then
        echo "NVIDIA device nodes did not appear within ${wait_secs}s; whisper-server will start on the CPU" >&2
        exit 0
    fi
    sleep 0.5
done

waited=$(( SECONDS - start ))
if (( waited > 0 )); then
    echo "NVIDIA device nodes ready after ${waited}s"
fi
exit 0
