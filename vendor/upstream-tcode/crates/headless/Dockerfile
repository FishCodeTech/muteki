# syntax=docker/dockerfile:1
# Build from the repository root:
#   docker build -f crates/headless/Dockerfile -t tcode-headless .
# Releases use --build-arg BINARY_SOURCE=release and the same Linux binary as
# the downloadable archives, staged in dist/<amd64|arm64>/tcode-headless.
ARG BINARY_SOURCE=source

FROM rust:1-trixie AS source
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        binaryen clang cmake libssl-dev libzstd-dev pkg-config python3 \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add wasm32-unknown-unknown
# crates/web/build.sh requires the CLI that matches the locked wasm-bindgen.
COPY Cargo.lock /tmp/Cargo.lock
RUN version=$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;q}' /tmp/Cargo.lock) \
    && cargo install wasm-bindgen-cli --version "$version" --locked
WORKDIR /src
COPY . .
ARG TARGETARCH
RUN --mount=type=cache,id=tcode-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=tcode-target-${TARGETARCH},target=/src/target,sharing=locked \
    crates/web/build.sh \
    && cargo build -p tcode-headless --features web --release --locked \
    && cp target/release/tcode-headless /tcode-headless

FROM scratch AS release
ARG TARGETARCH
COPY --chmod=755 dist/${TARGETARCH}/tcode-headless /tcode-headless

FROM ${BINARY_SOURCE} AS binary

FROM node:24-trixie-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash ca-certificates curl git libssl3t64 libzstd1 openssh-client \
        procps ripgrep tini tzdata util-linux xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && groupmod --new-name tcode node \
    && usermod --login tcode --home /data/home --shell /bin/bash node
COPY crates/headless/container/agents.json crates/headless/container/install-agents.mjs /opt/tcode/
# One global npm prefix per CPU architecture holds the selected npm CLIs and
# every other global install. /data/agents/bin links the CLIs kept elsewhere
# (Cursor's, Grok's current build); $HOME/.local/bin stays off PATH, so an
# unselected Cursor is not on it.
ARG TARGETARCH
ENV HOME=/data/home \
    TCODE_DATA_DIR=/data/tcode \
    SHELL=/bin/bash \
    LANG=C.UTF-8 \
    NPM_CONFIG_PREFIX=/data/agents/${TARGETARCH}/npm \
    NPM_CONFIG_CACHE=/data/agents/npm-cache \
    PATH=/data/agents/bin:/data/bin:/data/agents/${TARGETARCH}/npm/bin:$PATH
LABEL org.opencontainers.image.title="tcode-headless" \
      org.opencontainers.image.description="Tcode headless machine with the browser app, for NAS and homelab hosts" \
      org.opencontainers.image.licenses="MIT"
COPY --from=binary /tcode-headless /usr/local/bin/tcode-headless
COPY --chmod=755 crates/headless/container/entrypoint.sh /opt/tcode/entrypoint.sh
COPY crates/headless/container/profile.sh /etc/profile.d/tcode.sh
WORKDIR /projects
VOLUME ["/data"]
EXPOSE 47420/tcp 47420/udp
# A first start installs the selected CLIs before the listener opens.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10m --start-interval=5s \
    CMD curl -fsS -o /dev/null http://127.0.0.1:47420/auth/state || exit 1
ENTRYPOINT ["/usr/bin/tini", "--", "/opt/tcode/entrypoint.sh"]
CMD ["serve", "--browser-listen", "0.0.0.0:47420"]
