#!/usr/bin/env bash
# Applies or checks patches from orbit-cx/patches/*.patch in numerical order.
# Usage:
#   orbit-cx/scripts/apply-patches.sh          # apply all patches to working tree
#   orbit-cx/scripts/apply-patches.sh --check  # check if patches apply cleanly without changing tree
#   orbit-cx/scripts/apply-patches.sh --reverse# reverse applied patches
#   orbit-cx/scripts/apply-patches.sh --am     # apply as commits using git am
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
patch_dir="$(cd "$script_dir/../patches" && pwd)"

cd "$repo_root"

mode="apply"
if [ $# -gt 0 ]; then
  case "$1" in
    --check)
      mode="check"
      ;;
    --reverse|-R)
      mode="reverse"
      ;;
    --am)
      mode="am"
      ;;
    *)
      echo "Unknown option: $1" >&2
      echo "Usage: $0 [--check | --reverse | --am]" >&2
      exit 1
      ;;
  esac
fi

patches=("$patch_dir"/*.patch)
if [ ! -e "${patches[0]}" ]; then
  echo "No patches found in $patch_dir" >&2
  exit 1
fi

case "$mode" in
  check)
    echo "Checking ${#patches[@]} patches sequentially against index..."
    tmp_index=$(mktemp)
    git_dir=$(git rev-parse --git-dir)
    cp "$git_dir/index" "$tmp_index"
    export GIT_INDEX_FILE="$tmp_index"
    cleanup() { rm -f "$tmp_index"; }
    trap cleanup EXIT
    for p in "${patches[@]}"; do
      echo "  Checking $(basename "$p")..."
      git apply --cached "$p"
    done
    rm -f "$tmp_index"
    trap - EXIT
    echo "All patches apply cleanly."
    ;;
  apply)
    echo "Applying ${#patches[@]} patches..."
    for p in "${patches[@]}"; do
      echo "  Applying $(basename "$p")..."
      git apply "$p"
    done
    echo "All patches applied successfully."
    ;;
  reverse)
    echo "Reversing ${#patches[@]} patches in reverse order..."
    for (( i=${#patches[@]}-1; i>=0; i-- )); do
      p="${patches[i]}"
      echo "  Reversing $(basename "$p")..."
      git apply -R "$p"
    done
    echo "All patches reversed successfully."
    ;;
  am)
    echo "Applying ${#patches[@]} patches with git am..."
    git am "${patches[@]}"
    echo "All patches committed successfully."
    ;;
esac
