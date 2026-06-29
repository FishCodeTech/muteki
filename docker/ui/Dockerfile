# muteki Next UI (command deck) — P2-v3 compose `ui` service.
#
# Serves the operator UI and proxies /api → the web-api service (MUTEKI_BACKEND).
# Uses Next standalone output (next.config.mjs `output: "standalone"`).
#
# Build context = the UI dir:
#   docker build -f docker/ui/Dockerfile -t muteki-ui apps/web/ui
# Usually built via docker-compose.yml at the repo root.

FROM node:20-slim AS build
WORKDIR /ui
COPY package.json package-lock.json* /ui/
RUN npm ci || npm install
COPY . /ui
# next.config.mjs rewrites() reads process.env.MUTEKI_BACKEND when the module is
# evaluated, and `output: "standalone"` FREEZES that value into the bundled
# server.js at BUILD time — it is NOT re-read at runtime. So the proxy target must
# be baked here via a build arg, defaulting to the compose service alias. (Earlier
# this was wrongly assumed runtime-configurable; the standalone server hard-codes
# whatever MUTEKI_BACKEND was at build → it fell back to 127.0.0.1 and the UI
# proxied to itself.) Override for a non-compose target: --build-arg MUTEKI_BACKEND=...
ARG MUTEKI_BACKEND=http://web-api:8000
ENV MUTEKI_BACKEND=${MUTEKI_BACKEND}
RUN npm run build
# Ensure a public/ dir exists so the COPY in the run stage always has a source.
# This UI ships no static assets today; Dockerfile COPY is not shell, so a
# `|| true` on the COPY line does NOT swallow a missing-source error — create
# the dir here instead. (Harmless when real assets are added later.)
RUN mkdir -p /ui/public

FROM node:20-slim AS run
WORKDIR /ui
ENV NODE_ENV=production
# Standalone bundle: the server + its minimal node_modules, plus static assets.
COPY --from=build /ui/.next/standalone /ui/
COPY --from=build /ui/.next/static /ui/.next/static
COPY --from=build /ui/public /ui/public
EXPOSE 3001
# The standalone server honors PORT; default it to 3001 (the operator's port).
ENV PORT=3001 HOSTNAME=0.0.0.0
CMD ["node", "server.js"]
