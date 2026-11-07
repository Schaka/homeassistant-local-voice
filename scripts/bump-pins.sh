#!/usr/bin/env bash
# Looks up the newest upstream version of each pinned native component and
# rewrites the pin in docker-bake.hcl, the Dockerfile and docs/configuration.md.
# The workflow "Bump pins" runs it weekly and puts all changes in one pull
# request. It needs the GitHub CLI (`gh`).
#
#   scripts/bump-pins.sh --check            # list outdated pins, change nothing
#   scripts/bump-pins.sh                    # update all pins in the working tree
#   scripts/bump-pins.sh llamacpp parakeet  # update only these components
#
# Components: llamacpp, ikllama, audiocpp, parakeet, searxng, hush. The Rust
# toolchain, cargo-chef and the Hush model bundle are not handled here (skill
# bump-native-engine).
set -euo pipefail
cd "$(dirname "$0")/.."

check=0
if [ "${1:-}" = "--check" ]; then check=1; shift; fi
components=("$@")
if [ ${#components[@]} -eq 0 ]; then components=(llamacpp ikllama audiocpp parakeet searxng hush); fi

variable() {
  case "$1" in
    llamacpp) echo LLAMACPP_BUILD ;;
    ikllama) echo IKLLAMA_COMMIT ;;
    audiocpp) echo AUDIOCPP_COMMIT ;;
    parakeet) echo PARAKEET_VERSION ;;
    searxng) echo SEARXNG_COMMIT ;;
    hush) echo HUSH_COMMIT ;;
    *) echo "unknown component: $1" >&2; return 1 ;;
  esac
}

# The newest version that the Dockerfile can download. llama.cpp and
# parakeet.cpp need a release with the Vulkan archive. The "latest" release of
# llama.cpp is not the newest build tag, so the list is filtered.
latest() {
  case "$1" in
    llamacpp)
      gh api 'repos/ggml-org/llama.cpp/releases?per_page=30' --jq '
        [.[] | select(.tag_name | test("^b[0-9]+$"))
             | select(any(.assets[]; .name | endswith("-bin-ubuntu-vulkan-x64.tar.gz")))
             | .tag_name]
        | max_by(ltrimstr("b") | tonumber) // empty' ;;
    parakeet)
      gh api 'repos/mudler/parakeet.cpp/releases?per_page=10' --jq '
        [.[] | select(.draft | not) | select(.prerelease | not)
             | select(any(.assets[]; .name | endswith("-lib-linux-vulkan-x64.tar.gz")))
             | .tag_name] | first // empty' ;;
    ikllama) gh api repos/ikawrakow/ik_llama.cpp/commits/HEAD --jq .sha ;;
    audiocpp) gh api repos/0xShug0/audio.cpp/commits/HEAD --jq .sha ;;
    searxng) gh api repos/searxng/searxng/commits/HEAD --jq .sha ;;
    hush) gh api repos/pulp-vision/Hush/commits/HEAD --jq .sha ;;
  esac
}

current() {
  sed -n "s/^variable \"$1\" { default = \"\\(.*\\)\" }/\\1/p" docker-bake.hcl
}

write() {
  local var="$1" new="$2"
  sed -i "s#^\\(variable \"$var\" { default = \"\\)[^\"]*#\\1$new#" docker-bake.hcl
  sed -i "s#^ARG $var=.*#ARG $var=$new#" Dockerfile
  sed -i "s#^\\(| \`$var\` | \`\\)[^\`]*#\\1$new#" docs/configuration.md
}

for name in "${components[@]}"; do
  var="$(variable "$name")"
  old="$(current "$var")"
  new="$(latest "$name")"
  if [ -z "$new" ]; then echo "$name: no downloadable release found, skipped" >&2; continue; fi
  if [ "$old" = "$new" ]; then echo "$name: $old is the newest"; continue; fi
  echo "$name: $var $old -> $new"
  if [ "$check" -eq 0 ]; then write "$var" "$new"; fi
done
