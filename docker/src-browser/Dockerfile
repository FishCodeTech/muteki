FROM node:latest
ARG DEBIAN_MIRROR=http://deb.debian.org
RUN sed -i "s|http://deb.debian.org|${DEBIAN_MIRROR%/}|g" /etc/apt/sources.list.d/debian.sources \
    && apt-get update && apt-get install -y --no-install-recommends chromium libnss3-tools \
    && rm -rf /var/lib/apt/lists/*
ARG NPM_REGISTRY=https://registry.npmjs.org
RUN npm config set registry "$NPM_REGISTRY" \
    && npm install -g --allow-scripts=agent-browser agent-browser \
    && agent-browser --version \
    && chromium --version
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/bin/chromium
ENTRYPOINT ["sleep", "infinity"]
