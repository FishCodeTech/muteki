# Standalone Web image

On the target Linux architecture, build the Web release tar and image:

```sh
node scripts/package-web.mjs
./docker/web/build.sh PATH_TO_THE_TAR_PRINTED_ABOVE
```

Run the Rust API and Next UI together with the current Worker image:

```sh
MUTEKI_HOST_DATA_ROOT=/absolute/path/to/muteki-data \
MUTEKI_WEB_PASSWORD='your password' \
docker compose -f docker-compose.web.yml up -d --wait
```

The data directory is mounted at the same absolute path inside the Web
container. Worker containers started through the host Docker socket can then
bind the same workspace paths. The Web service and its Workers share the
`MUTEKI_WORKER_NETWORK` bridge network; the Worker connects back to the Rust
control receiver at the `web` service name. The UI is published on host
loopback at port 3101 by default (`MUTEKI_WEB_BIND` and `MUTEKI_WEB_PORT`
change the host bind address and port).

The Web gateway serves HTTP by default and does not force HTTPS. Put your own
reverse proxy (such as Nginx or Caddy) in front for HTTPS, or optionally mount
a certificate and key and pass both `MUTEKI_WEB_TLS_CERT` and
`MUTEKI_WEB_TLS_KEY`.
