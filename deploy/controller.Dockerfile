# Minimal image for an MPC hardware controller (a static musl binary + its JSON5 config).
#
# The config is **bind-mounted** at runtime (see deploy/run-growatt.sh / run-publisher.sh), so the
# `armed` flag and host/topic edits take effect on restart with no image rebuild — and an armed config
# never gets baked into an image. Build it with the static binary copied next to this file:
#   docker build -f controller.Dockerfile --build-arg BIN=mpc-controller-growatt -t mpc-growatt .
#   docker build -f controller.Dockerfile --build-arg BIN=mpc-plan-publisher    -t mpc-publisher .
FROM alpine:3.20
WORKDIR /app
ARG BIN
COPY ${BIN} /app/controller
# The controllers read their config from argv[1]; run scripts bind-mount the host file here.
ENTRYPOINT ["/app/controller", "/app/config.json5"]
