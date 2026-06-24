# muteki-pwn: linux/amd64 runtime for DYNAMIC pwn (running/debugging x86-64 ELFs).
# arm64 macOS can't natively run/gdb a Linux x86-64 binary; the pwn SDK shells
# into this container to develop the exploit locally, then fires at the remote.
FROM --platform=linux/amd64 ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 python3-pip python3-dev \
        gdb gcc libc6-dev \
        ruby file curl \
        && rm -rf /var/lib/apt/lists/*

# pwntools + gadget tooling (break system-packages: it's a throwaway CTF box)
RUN pip3 install --break-system-packages --no-cache-dir pwntools ROPgadget \
    && gem install --no-document one_gadget 2>/dev/null || true

# pwndbg for rich gdb (heap/tcache). Shallow clone, scripted setup.
RUN git clone --depth 1 https://github.com/pwndbg/pwndbg /opt/pwndbg 2>/dev/null \
    && cd /opt/pwndbg && ./setup.sh 2>/dev/null || echo "pwndbg setup best-effort"

WORKDIR /work
CMD ["sleep", "infinity"]
