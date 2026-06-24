# Minimal image for the read-only shadow brain, run alongside the loxone stack with
# `--restart unless-stopped` for crash + reboot persistence.
#
# The binary is a static musl build, so a tiny base works. Build it first on the host:
#   PATH=<zig>:$PATH cargo zigbuild --release \
#       --target x86_64-unknown-linux-musl --features vendored-tls
# then copy target/x86_64-unknown-linux-musl/release/mpc_home_control + model.json5 +
# config.json5 next to this Dockerfile and `docker build`.
#
# Run on the loxone `caddy_net` network so it reaches influx by service name; the binary reads
# INFLUXDB_TOKEN, INFLUX_HOST (-> http://influxdb:8086) and MPC_BIND (-> 0.0.0.0) from the env:
#   docker run -d --name mpc-shadow --restart unless-stopped --network caddy_net \
#       -e INFLUX_HOST=http://influxdb:8086 -e MPC_BIND=0.0.0.0 \
#       -e INFLUXDB_TOKEN="$(grep '^INFLUXDB_TOKEN=' .../loxone_smart_home/.env | cut -d= -f2-)" \
#       -p 127.0.0.1:3000:3000 mpc-shadow
FROM alpine:3.20
WORKDIR /app
COPY mpc_home_control model.json5 config.json5 ./
# Mount point for the forward-prediction snapshots (MPC_FORECAST_STORE); run-container.sh bind-mounts
# a host dir here so the /api/forecast/validation history survives container recreation.
RUN mkdir -p /app/data
ENV MPC_BIND=0.0.0.0
EXPOSE 3000
ENTRYPOINT ["/app/mpc_home_control", "serve"]
