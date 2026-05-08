#!/usr/bin/env bash
# Fixture wt: switch-create fails, list reports absent.
case "$1" in
  switch-create)
    echo "wt: simulated switch-create failure" >&2
    exit 7
    ;;
  list)
    # Empty output -> no worktree found.
    exit 0
    ;;
  remove)
    exit 0
    ;;
  *)
    echo "wt: unknown subcommand $1" >&2
    exit 1
    ;;
esac
