#!/usr/bin/env bash
# Run in the verified FFmpeg-Builds checkout. These are the same stage
# predicates and cache keys used by upstream generate.sh for this variant.
set -e
source util/vars.sh win64 gpl 8.1
for stage in scripts.d/*.sh scripts.d/*/*.sh; do
  (
    SELF="$stage"
    STAGENAME="$(basename "$stage" .sh)"
    source util/dl_functions.sh
    source "$stage"
    ffbuild_enabled || exit 0
    download="$(ffbuild_dockerdl)"
    if [[ -n "$download" ]]; then
      hash="$(sha256sum <<<"$download")"
      echo "${STAGENAME}_${hash%% *}.tar.xz"
    fi
  )
done
