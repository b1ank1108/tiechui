#!/bin/sh
cat > "$1"
printf '%s' "$2"
printf '%s' "$3" >&2
exit "$4"
