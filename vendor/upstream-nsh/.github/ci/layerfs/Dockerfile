# The SDK and daemon need Linux; this image also supplies NSH's Rust tools.
FROM rust:1.98.1-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546
RUN apt-get update \
 && apt-get install -y --no-install-recommends nodejs clang libclang-dev fuse3 \
 && rm -rf /var/lib/apt/lists/*
