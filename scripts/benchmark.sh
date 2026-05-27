#!/usr/bin/env bash
# STT benchmark for Step 1: measure whisper.cpp inference latency per model.
#
# Usage:
#   scripts/benchmark.sh record <name>      # record a 16kHz mono sample via pw-record
#   scripts/benchmark.sh run                # transcribe every sample with every model
#
# Env overrides:
#   WHISPER_DIR   whisper.cpp checkout (default: ~/.local/share/whisper.cpp)
#   SAMPLES_DIR   where samples live   (default: ./samples)
#   MODELS        space-separated ggml model basenames to test
set -euo pipefail

WHISPER_DIR="${WHISPER_DIR:-$HOME/.local/share/whisper.cpp}"
SAMPLES_DIR="${SAMPLES_DIR:-./samples}"
WHISPER_CLI="$WHISPER_DIR/build/bin/whisper-cli"
MODELS_DIR="$WHISPER_DIR/models"
MODELS="${MODELS:-ggml-tiny.bin ggml-small.bin ggml-medium-q5_0.bin ggml-large-v3-turbo-q5_0.bin}"

die() { echo "benchmark: $*" >&2; exit 1; }

cmd_record() {
  local name="${1:-}"
  [ -n "$name" ] || die "usage: benchmark.sh record <name>"
  mkdir -p "$SAMPLES_DIR"
  local out="$SAMPLES_DIR/$name.wav"
  echo "Recording to $out — Ctrl-C to stop."
  pw-record --rate 16000 --channels 1 --format s16 "$out"
}

cmd_run() {
  [ -x "$WHISPER_CLI" ] || die "whisper-cli not found at $WHISPER_CLI (build whisper.cpp first)"
  shopt -s nullglob
  local samples=("$SAMPLES_DIR"/*.wav)
  [ "${#samples[@]}" -gt 0 ] || die "no .wav samples in $SAMPLES_DIR (use: benchmark.sh record <name>)"

  for model in $MODELS; do
    local mpath="$MODELS_DIR/$model"
    [ -f "$mpath" ] || { echo "skip $model (not downloaded)"; continue; }
    for wav in "${samples[@]}"; do
      echo "=== model=$model sample=$(basename "$wav") ==="
      local start end
      start=$(date +%s.%N)
      "$WHISPER_CLI" -m "$mpath" -f "$wav" -l auto -np -nt
      end=$(date +%s.%N)
      printf "latency: %.2fs\n\n" "$(echo "$end - $start" | bc)"
    done
  done
}

case "${1:-}" in
  record) shift; cmd_record "$@" ;;
  run)    shift; cmd_run "$@" ;;
  *)      die "usage: benchmark.sh {record <name>|run}" ;;
esac
