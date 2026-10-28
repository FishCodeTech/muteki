FROM node:24-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends libnss3-tools \
    && rm -rf /var/lib/apt/lists/*
RUN npm install -g agent-browser@0.38.1 \
    && npx playwright install --with-deps chromium \
    && CHROME_DIR="$(find /root/.cache/ms-playwright -maxdepth 2 -type d -name 'chrome-linux*' | head -n 1)" \
    && test -n "$CHROME_DIR" \
    && cp -a "$CHROME_DIR" /opt/chromium \
    && chmod -R a+rx /opt/chromium \
    && ln -sf /opt/chromium/chrome /usr/bin/chromium
ENV AGENT_BROWSER_EXECUTABLE_PATH=/usr/bin/chromium
ENTRYPOINT ["sleep", "infinity"]
