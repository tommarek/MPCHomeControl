# Tiny image for the read-only mqtt-source sidecar (subscribes MQTT, serves the latest values over
# HTTP for the MPC to pull). Build the static musl binary first:
#   cargo zigbuild --release --target x86_64-unknown-linux-musl -p mpc-adapter-mqtt-source
# then copy target/x86_64-unknown-linux-musl/release/mpc-adapter-mqtt-source + mqtt-source.json5 next
# to this Dockerfile and `docker build`. Run on the loxone caddy_net so it reaches `mosquitto` and the
# MPC reaches it by name:
#   docker run -d --name mpc-mqtt-source --restart unless-stopped --network caddy_net mpc-mqtt-source
FROM alpine:3.20
WORKDIR /app
COPY mpc-adapter-mqtt-source mqtt-source.json5 ./
EXPOSE 8088
ENTRYPOINT ["/app/mpc-adapter-mqtt-source", "/app/mqtt-source.json5"]
