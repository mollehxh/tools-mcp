#!/bin/sh
set -eu
for tool in git gh rg fd jq curl wget ssh tmux cc c++ make cmake python3 node npm rustc cargo go; do
  path=$(command -v "$tool")
  case "$tool" in
    ssh|tmux) output=$($tool -V 2>&1) ;;
    go) output=$($tool version 2>&1) ;;
    *) output=$($tool --version 2>&1) ;;
  esac
  version=$(printf '%s\n' "$output" | sed -n '1p')
  [ -n "$version" ]
  printf '%s\t%s\t%s\n' "$tool" "$path" "$version"
done
