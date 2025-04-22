#!/usr/bin/env bash
# quick script to spin up a mosquitto server, subscribe to the topic to which
# the esp sends, and toggle the light on and off via the topic to which esp
# subscribes

set -Eeuf -o pipefail

main() {
  nix develop -c bash << 'EOF'
. .env

mosquitto -c mosquitto.conf &
msqto=$!
trap 'kill "${msqto}"' EXIT

until nc -zv 127.0.0.1 1883; do
  sleep 0.1
done

mosquitto_sub -h 127.0.0.1 -p 1883 -t "${PUBLISH_TOPIC}" &
msqto_sub=$!
trap 'kill "${msqto_sub}"' EXIT

while :; do
  mosquitto_pub -h 127.0.0.1 -p 1883 -t "${RECEIVE_TOPIC}" -m 1
  sleep 1
  mosquitto_pub -h 127.0.0.1 -p 1883 -t "${RECEIVE_TOPIC}" -m 0
  sleep 1
done
EOF
}
main "$@"
