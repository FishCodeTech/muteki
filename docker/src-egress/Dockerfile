FROM python:3.13-alpine
RUN apk add --no-cache iptables ip6tables iproute2 openssl
COPY gateway.py /usr/local/bin/src-egress-gateway
COPY http_proxy.py /usr/local/bin/http_proxy.py
COPY proxy_process.py /usr/local/bin/proxy_process.py
RUN chmod 755 /usr/local/bin/src-egress-gateway /usr/local/bin/proxy_process.py
ENTRYPOINT ["/usr/local/bin/src-egress-gateway"]
