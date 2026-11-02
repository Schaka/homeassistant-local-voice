#!/usr/bin/env bash
# Starts the LLM engine with exactly the command line that assistant-hub uses,
# asks the Assist questions of src/llm/assist.json and writes a JSON report.
# Run it once per engine, then compare the reports:
#   assistant-hub eval compare mainline.json candidate.json
#
#   LLM_MODEL_FILENAME=Ling-3.0-tiny-Q4_K_M.gguf LLM_DIR=/data/llm \
#     scripts/eval-llm.sh /data/eval/mainline.json
#
# The LLM_* variables are the ones of docs/configuration.md. LLAMA_SERVER names
# the engine binary. TASKSET (for example "taskset -c 0-5") pins the CPUs. REPEAT
# sets how often each question is asked (default 3). HUB is the assistant-hub
# binary (default: target/release or target/debug of this repository).
set -euo pipefail
out=${1:?path of the JSON report}
cd "$(dirname "$0")/.."
hub=${HUB:-$({ ls target/release/assistant-hub target/debug/assistant-hub 2>/dev/null || true; } | head -1)}
[ -x "$hub" ] || { echo "build assistant-hub first (cargo build --release)" >&2; exit 1; }

spec=$("$hub" llm-command)
mapfile -t cmd < <(jq -r '[.program] + .args | .[]' <<<"$spec")
port=$(jq -r .port <<<"$spec")
env_args=()
while IFS= read -r kv; do env_args+=("$kv"); done < <(jq -r '.env[] | "\(.[0])=\(.[1])"' <<<"$spec")
unset_args=()
while IFS= read -r k; do unset_args+=(-u "$k"); done < <(jq -r '.env_remove[]' <<<"$spec")

log="${out%.json}.engine.log"
# shellcheck disable=SC2086
env "${unset_args[@]}" "${env_args[@]}" ${TASKSET:-} "${cmd[@]}" > "$log" 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT
for _ in $(seq 1 600); do
    curl -sf --max-time 2 "http://127.0.0.1:$port/health" > /dev/null && break
    kill -0 "$pid" 2>/dev/null || { tail -20 "$log" >&2; echo "the engine exited" >&2; exit 1; }
    sleep 1
done
curl -sf --max-time 2 "http://127.0.0.1:$port/health" > /dev/null || { echo "the engine did not start" >&2; exit 1; }
"$hub" eval llm --base-url "http://127.0.0.1:$port/v1" --repeat "${REPEAT:-3}" \
    --temperature "${TEMPERATURE:-0.7}" --top-p "${TOP_P:-1.0}" --out "$out"
