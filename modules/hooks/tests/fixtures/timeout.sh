#!/bin/sh
cat > /dev/null
sleep 30 &
printf '%s' "$!" > "$1"
wait
