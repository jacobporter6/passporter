#!/usr/bin/env bash
# Builds whisper.cpp's whisper-server (CUDA if available, else CPU),
# downloads the model, and installs the systemd user unit that serves it on
# 127.0.0.1:8178 for passporter-bridge (design §6.3).
#
#   deploy/whisper/build.sh            # from anywhere inside the repo checkout
#
# What it does:
#   1. checks the toolchain (cmake, a C++ compiler, git, ffmpeg, plus nvcc
#      for CUDA) and stops with install hints if anything is missing;
#   2. clones (or updates) whisper.cpp at WHISPER_REF into ~/.local/src;
#   3. builds whisper-server, statically linked, for this machine's GPU
#      (CUDA_ARCH, default "native") or for the CPU;
#   4. installs it to ~/.local/bin and the model to
#      ~/.local/share/passporter/whisper/;
#   5. installs, enables and (re)starts whisper-server.service.
#
# Safe to re-run: it rebuilds, reinstalls and restarts; the model is only
# downloaded once.
#
# Overrides:
#   WHISPER_BACKEND  cuda or cpu (default: cuda when nvcc is found, else cpu)
#   WHISPER_MODEL    default large-v3-turbo with CUDA, base.en on the CPU
#                    (small.en is a good CPU middle ground)
#   CUDA_ARCH        default "native" (the GPU in this machine); set it to
#                    build elsewhere, e.g. 86 for an RTX 30xx, 89 for an RTX 40xx
#   WHISPER_REF      the whisper.cpp git tag
set -euo pipefail

# Some distributions (Arch) install CUDA to /opt/cuda and only add it to
# PATH for new login shells.
if ! command -v nvcc >/dev/null 2>&1 && [[ -x /opt/cuda/bin/nvcc ]]; then
    export PATH="/opt/cuda/bin:${PATH}"
fi
if [[ -z "${WHISPER_BACKEND:-}" ]]; then
    if command -v nvcc >/dev/null 2>&1; then WHISPER_BACKEND=cuda; else WHISPER_BACKEND=cpu; fi
fi
case "$WHISPER_BACKEND" in
    cuda) default_model=large-v3-turbo ;;
    cpu) default_model=base.en ;;
    *) echo "WHISPER_BACKEND must be cuda or cpu, not ${WHISPER_BACKEND}" >&2; exit 2 ;;
esac

WHISPER_REPO="https://github.com/ggml-org/whisper.cpp.git"
WHISPER_REF="${WHISPER_REF:-v1.7.6}"
WHISPER_MODEL="${WHISPER_MODEL:-$default_model}"
CUDA_ARCH="${CUDA_ARCH:-native}"

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
src="${HOME}/.local/src/whisper.cpp"
bin_dir="${HOME}/.local/bin"
model_dir="${HOME}/.local/share/passporter/whisper"
unit_dir="${HOME}/.config/systemd/user"

say() { printf '\n==> %s\n' "$*"; }

# --- 1. Toolchain -----------------------------------------------------------

missing=()
if [[ "$WHISPER_BACKEND" == cuda ]]; then
    command -v nvcc >/dev/null 2>&1 || missing+=(cuda)
fi
command -v cmake >/dev/null 2>&1 || missing+=(cmake)
command -v git >/dev/null 2>&1 || missing+=(git)
command -v g++ >/dev/null 2>&1 || missing+=(gcc)
# whisper-server --convert shells out to ffmpeg to decode WebM/Opus and AAC.
command -v ffmpeg >/dev/null 2>&1 || missing+=(ffmpeg)
command -v curl >/dev/null 2>&1 || command -v wget >/dev/null 2>&1 || missing+=(curl)
if ((${#missing[@]})); then
    cat >&2 <<MSG
Missing: ${missing[*]}

Install them with your package manager, then re-run this script. For example:

    Arch:           sudo pacman -S --needed cmake gcc git ffmpeg curl   (+ cuda)
    Debian/Ubuntu:  sudo apt install cmake g++ git ffmpeg curl          (+ nvidia-cuda-toolkit)
    Fedora:         sudo dnf install cmake gcc-c++ git ffmpeg curl      (+ cuda from NVIDIA's repo)

"cuda" means the CUDA toolkit (nvcc) on top of a working NVIDIA driver.
No NVIDIA GPU? Run with WHISPER_BACKEND=cpu.
MSG
    exit 1
fi

host_cxx=""
if [[ "$WHISPER_BACKEND" == cuda ]]; then
    if ! command -v nvidia-smi >/dev/null 2>&1 || ! nvidia-smi >/dev/null 2>&1; then
        echo "nvidia-smi cannot talk to the GPU: is the NVIDIA driver loaded? (or use WHISPER_BACKEND=cpu)" >&2
        exit 1
    fi
    # nvcc supports only some host compilers; some distributions ship a
    # matching older gcc and point NVCC_CCBIN at it. Use it if present.
    for candidate in "${NVCC_CCBIN:-}" /opt/cuda/bin/g++ /usr/bin/g++-14 /usr/bin/g++-13; do
        if [[ -x "$candidate" ]]; then
            host_cxx="$candidate"
            break
        fi
    done
    say "Toolchain: $(nvcc --version | tail -n 1)"
    [[ -n "$host_cxx" ]] && echo "CUDA host compiler: ${host_cxx}"
else
    say "Building for the CPU (no CUDA)"
fi

# --- 2. Source ----------------------------------------------------------------

say "whisper.cpp ${WHISPER_REF} in ${src}"
if [[ -d "${src}/.git" ]]; then
    git -C "$src" fetch --tags --quiet origin
else
    install -d -m 0755 "$(dirname "$src")"
    git clone --quiet "$WHISPER_REPO" "$src"
fi
git -C "$src" checkout --quiet "$WHISPER_REF"

# --- 3. Build -------------------------------------------------------------------

cmake_args=(
    -S "$src" -B "${src}/build"
    -DCMAKE_BUILD_TYPE=Release
    -DBUILD_SHARED_LIBS=OFF
    -DWHISPER_BUILD_EXAMPLES=ON
    -DWHISPER_BUILD_TESTS=OFF
)
if [[ "$WHISPER_BACKEND" == cuda ]]; then
    say "Building whisper-server (CUDA, architectures: ${CUDA_ARCH})"
    cmake_args+=(-DGGML_CUDA=ON -DCMAKE_CUDA_ARCHITECTURES="$CUDA_ARCH")
    [[ -n "$host_cxx" ]] && cmake_args+=(-DCMAKE_CUDA_HOST_COMPILER="$host_cxx")
else
    say "Building whisper-server (CPU)"
    cmake_args+=(-DGGML_CUDA=OFF)
fi
cmake "${cmake_args[@]}"
cmake --build "${src}/build" --config Release --target whisper-server -j "$(nproc)"

# --- 4. Install ------------------------------------------------------------------

say "Installing whisper-server to ${bin_dir}"
install -d -m 0755 "$bin_dir"
install -m 0755 "${src}/build/bin/whisper-server" "${bin_dir}/whisper-server.new"
mv -f "${bin_dir}/whisper-server.new" "${bin_dir}/whisper-server"

model="${model_dir}/ggml-${WHISPER_MODEL}.bin"
install -d -m 0700 "$model_dir"
if [[ -s "$model" ]]; then
    echo "Keeping the existing model ${model}."
else
    say "Downloading the ${WHISPER_MODEL} model"
    (cd "${src}/models" && sh ./download-ggml-model.sh "$WHISPER_MODEL")
    install -m 0644 "${src}/models/ggml-${WHISPER_MODEL}.bin" "$model"
fi
# The unit always loads model.bin, so switching models is a re-run away.
ln -sfn "$(basename "$model")" "${model_dir}/model.bin"

say "Installing the systemd user unit"
install -d -m 0755 "$unit_dir"
install -m 0644 "${repo}/deploy/whisper/whisper-server.service" "${unit_dir}/whisper-server.service"
systemctl --user daemon-reload
systemctl --user enable whisper-server.service
systemctl --user restart whisper-server.service
sleep 2
systemctl --user --no-pager --lines=15 status whisper-server.service || true

cat <<MSG

Done. whisper-server listens on 127.0.0.1:8178 (the bridge's default
[audio] whisper address). Useful commands:
  journalctl --user -u whisper-server -f        # logs (model load, device)
  curl -s 127.0.0.1:8178/inference -F file=@clip.wav -F response_format=json
MSG
