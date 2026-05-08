#!/usr/bin/env bash
# Fixture wt: switch-create + list succeed by maintaining a fake registry on
# disk under $ROKI_WT_FAKE_REGISTRY (set by the test). remove exits non-zero.
REG="${ROKI_WT_FAKE_REGISTRY:?ROKI_WT_FAKE_REGISTRY must be set by the test}"
case "$1" in
  switch-create)
    mkdir -p "$REG/$2"
    exit 0
    ;;
  list)
    if [ -d "$REG" ]; then
      for d in "$REG"/*/; do
        [ -d "$d" ] || continue
        name=$(basename "$d")
        # tab-separated: branch<TAB>path
        printf '%s\t%s\n' "$name" "$d"
      done
    fi
    exit 0
    ;;
  remove)
    echo "wt: simulated remove failure" >&2
    exit 9
    ;;
  *)
    echo "wt: unknown subcommand $1" >&2
    exit 1
    ;;
esac
