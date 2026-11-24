# Build the Linux addon and daemon from a macOS checkout.
FROM nsh-layerfs:1.0.0
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3 \
 && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install 1.85.1 --profile minimal
