FROM docker:cli AS docker-cli
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl libdav1d7 libstdc++6 git ripgrep python3 && rm -rf /var/lib/apt/lists/*
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
ARG RELEASE_ASSET=web-release.tar.gz
COPY ${RELEASE_ASSET} /tmp/release.tar.gz
RUN mkdir -p /opt/muteki && tar -xzf /tmp/release.tar.gz -C /opt/muteki --strip-components=1 && rm /tmp/release.tar.gz
WORKDIR /opt/muteki
ENV MUTEKI_UI_HOST=0.0.0.0 MUTEKI_DEPLOYMENT_MODE=compose
EXPOSE 3101
ENTRYPOINT ["/opt/muteki/bin/node", "/opt/muteki/launcher.mjs", "web"]
