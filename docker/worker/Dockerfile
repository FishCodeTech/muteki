# muteki worker image — clean Kali build (only public tooling, no baked login state).
# Ships the full Kali/CTF toolchain + offline knowledge base + the eight container
# engine CLIs (claude, codex, cursor, pi, omp, kimi, grok, opencode) + the
# in-container Runtime Control Plane supervisor (runtime_agent, PID1/ENTRYPOINT).
# Devin is host-local only; DeepSeek Harness is temporarily disabled and is not
# baked here. Credentials are injected at runtime from Credential Accounts.
#
# Formal builds ship only through .github/workflows/release-images.yml (native
# amd64/arm64). docker/worker/build.sh is a local development helper, not the
# release entrypoint. Do not docker push formal images from a workstation.
#
# Full-image size evaluation (MNT-09.13 / #173): after a GitHub Actions build,
# read the job summary / digests artifact, pull each arch by digest, and record
# `docker image inspect --format '{{.Size}}'`, build duration, and whether
# kali-linux-headless / Sage / offline knowledge trees justify a separate
# optional data pack. Do not require a local full rebuild for that decision.
#
# The platform is set by the BUILD COMMAND, NOT a const here:
# a `FROM --platform=amd64` const trips a BuildKit export bug on arm64 Docker Desktop
# with the containerd image store ("operating system is not supported" at load time,
# after the layers already exported) — and is what the FromPlatformFlagConstDisallowed
# warning flags. Leave FROM platform-less so it follows the build's --platform.

# GitHub Actions exports this stage first, then starts the final stage on a fresh
# runner from the pushed digest. Local builds keep using this stage directly.
ARG MUTEKI_WORKER_BASE=worker-base

# Pinned to a specific Kali rolling digest for reproducible builds (rolling:latest
# changes daily). Re-pin periodically: `docker pull kalilinux/kali-rolling:latest`
# then read the new digest from `docker images --digests`. Last pinned: 2026-06-17.
FROM kalilinux/kali-rolling@sha256:256893c92bbd289b07d9ef8a62e75f9c7cb3d9e570fb3d3725b2e86b9acd5728 AS worker-base

ARG TARGETARCH

# http.kali.org is a redirector and can send one apt transaction across several
# mirrors. A single unavailable mirror then stalls the complete image build. Use
# Kali's official CDN by default; build.sh exposes MUTEKI_KALI_MIRROR as an override.
ARG KALI_MIRROR="http://kali.download/kali"

ENV TZ=Asia/Shanghai \
    PLAYWRIGHT_MCP_BROWSER=chromium \
    CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    PYTHONUNBUFFERED=1

# 0) apt resilience for the Kali rolling mirror: retry transient fetch failures, and
#    SKIP the huge `Contents-*.gz` index (only apt-file needs it; it is NOT required
#    to install packages, and it's the file that fails a hash/size check when the
#    mirror is mid-sync — "File has unexpected size. Mirror sync in progress?" — which
#    would otherwise abort an `apt update` and the whole layer). Without it, installs
#    work fine off the Packages index.
RUN sed -i "s|http://http.kali.org/kali/|${KALI_MIRROR%/}/|g" \
      /etc/apt/sources.list.d/kali.sources && \
    kali_mirror_host="${KALI_MIRROR#*://}" && \
    kali_mirror_host="${kali_mirror_host%%/*}" && \
    printf 'Acquire::Retries "8";\nAcquire::Queue-Mode "host";\nAcquire::http::Pipeline-Depth "0";\nAcquire::https::Pipeline-Depth "0";\nAcquire::http::Proxy::%s "DIRECT";\nAcquire::https::Proxy::%s "DIRECT";\nAcquire::IndexTargets::deb::Contents-deb::DefaultEnabled "false";\nAcquire::IndexTargets::deb::Contents-deb-legacy::DefaultEnabled "false";\n' \
      "$kali_mirror_host" "$kali_mirror_host" \
      > /etc/apt/apt.conf.d/99muteki-robust

# 1) Kali headless metapackage — the broad tool base every track draws on.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt update && \
    apt -y install kali-linux-headless && \
    apt clean && \
    rm -rf /var/lib/apt/lists/*

# The release workflow overrides MUTEKI_WORKER_BASE with the architecture-specific
# digest exported by the base job. The default stage name preserves one-command
# local builds through docker/worker/build.sh. Keep this boundary early: BuildKit
# retains both compressed and unpacked base layers while extending a remote image.
FROM ${MUTEKI_WORKER_BASE} AS final

ARG TARGETARCH

# 2) User model: kali + NOPASSWD sudo. The worker runs as kali WITH full sudo so it
#    can install packages / change the system / start services while solving. This is
#    deliberate (solving ability first); the control socket is NOT a boundary against
#    the worker (see DESIGN §8).
RUN useradd -m -s /bin/bash kali && \
    echo "kali ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# 2b) Agent ergonomics: workers are supposed to be able to install missing tools on
# demand. Models sometimes type `apt-get install ...` without `sudo`; because
# /usr/local/bin is before /usr/bin, these wrappers transparently retry common
# package-manager commands through NOPASSWD sudo when the caller is not root.
RUN cat > /usr/local/sbin/muteki-auto-sudo <<'EOF' && \
    chmod +x /usr/local/sbin/muteki-auto-sudo && \
    ln -sf /usr/local/sbin/muteki-auto-sudo /usr/local/bin/apt-get && \
    ln -sf /usr/local/sbin/muteki-auto-sudo /usr/local/bin/apt && \
    ln -sf /usr/local/sbin/muteki-auto-sudo /usr/local/bin/dpkg
#!/bin/sh
name="$(basename "$0")"
real="/usr/bin/$name"
if [ "$(id -u)" = "0" ]; then
  exec "$real" "$@"
fi
exec sudo -n "$real" "$@"
EOF

# 3) CTF supplements + RE/forensics/crypto heavyweights. Split so a single flaky
#    mirror doesn't void the whole layer. (kali-linux-headless already pulls many of
#    these; listing the ones we depend on makes the image self-documenting and pins
#    the big optional ones — sagemath/volatility3.)
#    GitHub CLI is not in Kali rolling; it is installed from a pinned GitHub
#    release after this layer.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt update && \
    apt install -y --no-install-recommends \
      python3 python3-pip python3-venv python3-pwntools \
      curl wget git ca-certificates gnupg xz-utils unzip zip xxd file jq \
      ripgrep fd-find cmake pkg-config build-essential npm \
      nmap masscan ffuf gobuster dirb nikto whatweb sqlmap nuclei \
      dnsutils iputils-ping net-tools netcat-traditional ncat socat \
      proxychains4 chisel tmux rlwrap sshpass openssh-client openvpn openssl \
      gdb gdb-multiarch radare2 binutils patchelf cpio qemu-user-static \
      strace ltrace upx-ucl tcpdump hydra john hashcat \
      tshark binwalk foremost exiftool steghide sleuthkit poppler-utils \
      squashfs-tools tesseract-ocr tesseract-ocr-eng \
      redis-tools default-mysql-client postgresql-client php-cli \
      default-jdk-headless mono-mcs ruby ruby-dev && \
    apt clean && rm -rf /var/lib/apt/lists/*

# Mobile analysis packages vary across Kali rolling transitions. Install each one
# independently so one temporarily missing package does not remove the rest of the
# CTF toolchain from the image.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt update && \
    for package in jadx apktool aapt apksigner zipalign adb dex2jar; do \
      apt install -y --no-install-recommends "$package" || \
        echo "optional mobile package unavailable: $package"; \
    done && \
    apt clean && rm -rf /var/lib/apt/lists/*

# SageMath/volatility3 are not apt-installable in current Kali rolling. Sage comes
# from conda-forge (step 4b), while volatility3 comes from pip (step 4).

# SecLists wordlists. Kali's seclists .deb intermittently fails to fetch from the
# CDN mirror (the same flaky pool path that broke the old Ghidra layer), so clone
# the upstream repo — pinned to a release tag — into the exact /usr/share/seclists
# location the package would provide. Shallow, .git stripped afterwards.
ARG SECLISTS_TAG=2026.1
RUN git clone --depth=1 --branch "$SECLISTS_TAG" \
      https://github.com/danielmiessler/SecLists.git /tmp/seclists && \
    mkdir -p /usr/share/seclists && \
    cp -a /tmp/seclists/. /usr/share/seclists/ && \
    rm -rf /tmp/seclists /usr/share/seclists/.git

# GitHub CLI is not packaged in current Kali rolling. Install a pinned official
# Linux tarball so `gh` is on PATH without adding a second apt source.
ARG GH_VERSION=2.89.0
ARG GH_SHA256_AMD64=d0422caade520530e76c1c558da47daebaa8e1203d6b7ff10ad7d6faba3490d8
ARG GH_SHA256_ARM64=9e64a623dfc242990aa5d9b3f507111149c4282f66b68eaad1dc79eeb13b9ce5
RUN case "$TARGETARCH" in \
      amd64) gh_arch="amd64"; gh_sha="$GH_SHA256_AMD64" ;; \
      arm64) gh_arch="arm64"; gh_sha="$GH_SHA256_ARM64" ;; \
      *) echo "unsupported GitHub CLI architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL --retry 8 --retry-all-errors -o /tmp/gh.tar.gz \
      "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_${gh_arch}.tar.gz" && \
    echo "${gh_sha}  /tmp/gh.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/gh.tar.gz -C /tmp && \
    install -m 0755 "/tmp/gh_${GH_VERSION}_linux_${gh_arch}/bin/gh" /usr/local/bin/gh && \
    rm -rf /tmp/gh.tar.gz "/tmp/gh_${GH_VERSION}_linux_${gh_arch}" && \
    gh --version

# 4) CTF python stack (system-wide, so any worker/user picks it up). Pwntools stays
#    on Kali's Python 3.14-compatible package; its PyPI dependency currently selects
#    unicorn 2.1.2, which does not compile on this rolling base. volatility3 is here
#    (pip) since Kali no longer packages it — gives the `vol`/`vol3` commands.
#    --ignore-installed: angr pulls a newer mpmath, but Kali's mpmath was apt-
#    installed with no pip RECORD file, so pip refuses to uninstall it and aborts
#    ("Cannot uninstall mpmath ... no RECORD file"). --ignore-installed tells pip to
#    install its versions ALONGSIDE the apt ones instead of trying to remove them.
RUN pip3 install --break-system-packages --no-cache-dir --ignore-installed \
      requests beautifulsoup4 lxml pymongo \
      pycryptodome pycryptodomex termcolor ratelimit \
      sympy gmpy2 z3-solver ropgadget angr volatility3 \
      impacket boto3 mmh3 jwcrypto ecdsa \
      capstone scapy pillow pyasn1 pefile xdis dnfile \
      python-registry regipy psycopg2-binary

# Large specialist Python packages have native/runtime dependencies that can be
# temporarily unavailable on Kali rolling. Keep them best-effort, while the core
# toolchain above remains a hard build requirement. Do not pip-install pwntools:
# the apt package already provides it, and the PyPI tree pulls unicorn 2.1.2.
RUN for package in \
      numpy scipy awscli unicorn web3 eth-account pyevmasm \
      androguard frida-tools objection semgrep bandit mitmproxy \
      slither-analyzer py-solc-x \
      cysignals fpylll playwright; do \
      pip3 install --break-system-packages --no-cache-dir --ignore-installed "$package" || \
        echo "optional Python package unavailable: $package"; \
    done && \
    python3 -m playwright install --with-deps chromium || \
      echo "optional Playwright Chromium unavailable"

# OpenCV 5.0.0.93 currently raises SIGILL on linux/arm64 under Docker Desktop.
# Resolve RapidOCR and ONNX Runtime together with the last known working ARM64
# OpenCV wheel so pip cannot upgrade it again while resolving RapidOCR's deps.
RUN pip3 install --break-system-packages --no-cache-dir --ignore-installed \
      rapidocr-onnxruntime onnxruntime \
      opencv-python==4.10.0.84 opencv-python-headless==4.10.0.84 && \
    python3 -c 'import cv2, rapidocr_onnxruntime; assert cv2.__version__ == "4.10.0"'

# Ruby helpers commonly used by pwn and steganography workers. Keep them
# best-effort so a native gem compile failure does not void the image.
RUN for gem_name in one_gadget seccomp-tools zsteg; do \
      gem install --no-document "$gem_name" || \
        echo "optional Ruby gem unavailable: $gem_name"; \
    done

# Extra public CTF scripts that Kali rolling does not package. Pin commit/tag and
# verify SHA-256 before installing onto PATH.
ARG JWT_TOOL_VERSION=2.3.0
ARG JWT_TOOL_SHA256=0f8c32aa9f67cea4312e5df81d48f72dc5daf57f3cc0ce09d67db78e23fbe451
ARG GEF_COMMIT=18404074f469c70480aae071fbb77b75a2ed8507
ARG GEF_SHA256=04cdfe961f1e9151933d32cf6b548d9e6a76a1aef8b27c020c575b8d4264ed20
ARG PHP_FILTER_CHAIN_COMMIT=ce3cc76800827ad6f5a6438c69882068f3e0d71a
ARG PHP_FILTER_CHAIN_SHA256=a53866355e80013af3c98adf87f902f3c4015e1a924056ac9bce34ef68b6bfeb
RUN mkdir -p /opt/jwt_tool /opt/gef && \
    curl -fsSL --retry 8 --retry-all-errors -o /tmp/jwt_tool.tar.gz \
      "https://github.com/ticarpi/jwt_tool/archive/refs/tags/v${JWT_TOOL_VERSION}.tar.gz" && \
    echo "${JWT_TOOL_SHA256}  /tmp/jwt_tool.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/jwt_tool.tar.gz -C /tmp && \
    cp -a "/tmp/jwt_tool-${JWT_TOOL_VERSION}/." /opt/jwt_tool/ && \
    printf '#!/bin/sh\nexec python3 /opt/jwt_tool/jwt_tool.py "$@"\n' \
      > /usr/local/bin/jwt_tool && \
    chmod +x /usr/local/bin/jwt_tool /opt/jwt_tool/jwt_tool.py && \
    curl -fsSL --retry 8 --retry-all-errors -o /opt/gef/gef.py \
      "https://raw.githubusercontent.com/hugsy/gef/${GEF_COMMIT}/gef.py" && \
    echo "${GEF_SHA256}  /opt/gef/gef.py" | sha256sum -c - && \
    printf 'source /opt/gef/gef.py\n' > /home/kali/.gdbinit && \
    chown kali:kali /home/kali/.gdbinit && \
    curl -fsSL --retry 8 --retry-all-errors -o /usr/local/bin/php_filter_chain_generator.py \
      "https://raw.githubusercontent.com/synacktiv/php_filter_chain_generator/${PHP_FILTER_CHAIN_COMMIT}/php_filter_chain_generator.py" && \
    echo "${PHP_FILTER_CHAIN_SHA256}  /usr/local/bin/php_filter_chain_generator.py" | sha256sum -c - && \
    chmod +x /usr/local/bin/php_filter_chain_generator.py && \
    ln -sf /usr/local/bin/php_filter_chain_generator.py /usr/local/bin/php_filter_chain_generator && \
    rm -rf /tmp/jwt_tool.tar.gz "/tmp/jwt_tool-${JWT_TOOL_VERSION}"

# Specialist tools not consistently packaged by Kali rolling. Each release is pinned
# and SHA-256/SHA-512-verified (same supply-chain rule as Miniforge/gh above) so the
# full image covers cloud/container audit, blockchain, Go/.NET reversing, Java
# deserialization and advanced steganography without runtime downloads.
ARG CLOUDFOX_VERSION=2.0.5
ARG CLOUDFOX_SHA256_AMD64=3cdc5a1a94ff14eb8df04d3dc8f9eca7db232a1315ceb8a7de26e4cb13e32fd5
ARG CLOUDFOX_SHA256_ARM64=fcebd90329a8bb2f61c00cfb131572a44483251da532f1363a0ceb56541cd4ca
ARG CDK_VERSION=1.5.2
ARG CDK_SHA256_AMD64=af751c690671ffc0da6380ef94a25df3dfc5911c448319f7f6b90df55cca7b7d
ARG CDK_SHA256_ARM64=828aede9a7bc193899b66e8c10ac10d24398cf79575e771d9a970d3f9a4cdd92
RUN case "$TARGETARCH" in \
      amd64) tool_arch=amd64; cloudfox_sha="$CLOUDFOX_SHA256_AMD64"; cdk_sha="$CDK_SHA256_AMD64" ;; \
      arm64) tool_arch=arm64; cloudfox_sha="$CLOUDFOX_SHA256_ARM64"; cdk_sha="$CDK_SHA256_ARM64" ;; \
      *) echo "unsupported specialist-tool architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL --retry 8 --retry-all-errors -o /tmp/cloudfox.zip \
      "https://github.com/BishopFox/cloudfox/releases/download/v${CLOUDFOX_VERSION}/cloudfox-linux-${tool_arch}.zip" && \
    echo "${cloudfox_sha}  /tmp/cloudfox.zip" | sha256sum -c - && \
    unzip -q /tmp/cloudfox.zip -d /tmp/cloudfox && \
    install -m 0755 "$(find /tmp/cloudfox -type f -name cloudfox | head -1)" /usr/local/bin/cloudfox && \
    curl -fsSL --retry 8 --retry-all-errors -o /usr/local/bin/cdk \
      "https://github.com/cdk-team/CDK/releases/download/v${CDK_VERSION}/cdk_linux_${tool_arch}" && \
    echo "${cdk_sha}  /usr/local/bin/cdk" | sha256sum -c - && \
    chmod 0755 /usr/local/bin/cdk && \
    rm -rf /tmp/cloudfox.zip /tmp/cloudfox

ARG FOUNDRY_VERSION=1.7.1
ARG FOUNDRY_SHA256_AMD64=cf7e688ed0c4c48adffca788b496076e31060b67ac5afe1e43dbb5499c20c88b
ARG FOUNDRY_SHA256_ARM64=c8fe8fa09ae3aba2c81b510c6f9da3a9d468029b9580e690b245b3f0aea687ae
RUN case "$TARGETARCH" in \
      amd64) tool_arch=amd64; foundry_sha="$FOUNDRY_SHA256_AMD64" ;; \
      arm64) tool_arch=arm64; foundry_sha="$FOUNDRY_SHA256_ARM64" ;; \
      *) echo "unsupported Foundry architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL --retry 8 --retry-all-errors -o /tmp/foundry.tar.gz \
      "https://github.com/foundry-rs/foundry/releases/download/v${FOUNDRY_VERSION}/foundry_v${FOUNDRY_VERSION}_linux_${tool_arch}.tar.gz" && \
    echo "${foundry_sha}  /tmp/foundry.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/foundry.tar.gz -C /usr/local/bin forge cast anvil && \
    rm -f /tmp/foundry.tar.gz
# NOTE: the Foundry tarball also ships a `chisel` (Solidity REPL); it is deliberately
# NOT extracted — /usr/local/bin/chisel would shadow the apt tunneling chisel.

# GoReSym upstream ships a Linux build for x86-64 only; skip it on arm64.
ARG GORE_VERSION=3.4
ARG GORE_SHA256=cdcbda1273de4a502044520af3cbdb73149e74847aa1c515b413de4243e972f4
RUN if [ "$TARGETARCH" = "amd64" ]; then \
      curl -fsSL --retry 8 --retry-all-errors -o /tmp/goresym.zip \
        "https://github.com/mandiant/GoReSym/releases/download/v${GORE_VERSION}/GoReSym-linux.zip" && \
      echo "${GORE_SHA256}  /tmp/goresym.zip" | sha256sum -c - && \
      unzip -q /tmp/goresym.zip -d /tmp/goresym && \
      install -m 0755 /tmp/goresym/GoReSym /usr/local/bin/GoReSym && \
      rm -rf /tmp/goresym.zip /tmp/goresym; \
    else \
      echo "GoReSym upstream ships linux x86-64 only; skipping on $TARGETARCH"; \
    fi

# Java exploitation jars + StegSolve. marshalsec's prebuilt jar lives on the repo's
# master branch (no release tag); ysoserial/JNDI/StegSolve use pinned release tags.
ARG YSOSERIAL_VERSION=0.0.6
ARG YSOSERIAL_SHA256=2c9bddd6a1a4ec66c1078ea97dacb61eb66d1c41aec7b6d21e3c72214ce170f1
ARG MARSHALSEC_SHA256=524115cfae28f11759856f2e10946305d686b018a85a01642b436ee28cbb52af
ARG JNDI_EXPLOIT_SHA256=53517fdc0d386c7dfd4546e9e83150f85dab0662dbcaf9bf0f9dd82fab3a07e9
ARG STEGSOLVE_VERSION=1.4
ARG STEGSOLVE_SHA256=2cc24c81d6731b9e5672d2e7ec129378705dfc9a87024c7a24ae740d5efa404f
RUN mkdir -p /opt/tools && \
    curl -fsSL --retry 8 --retry-all-errors -o /opt/tools/ysoserial.jar \
      "https://github.com/frohoff/ysoserial/releases/download/v${YSOSERIAL_VERSION}/ysoserial-all.jar" && \
    echo "${YSOSERIAL_SHA256}  /opt/tools/ysoserial.jar" | sha256sum -c - && \
    curl -fsSL --retry 8 --retry-all-errors -o /opt/tools/marshalsec.jar \
      "https://raw.githubusercontent.com/RandomRobbieBF/marshalsec-jar/master/marshalsec-0.0.3-SNAPSHOT-all.jar" && \
    echo "${MARSHALSEC_SHA256}  /opt/tools/marshalsec.jar" | sha256sum -c - && \
    curl -fsSL --retry 8 --retry-all-errors -o /opt/tools/JNDI-Injection-Exploit.jar \
      "https://github.com/welk1n/JNDI-Injection-Exploit/releases/download/v1.0/JNDI-Injection-Exploit-1.0-SNAPSHOT-all.jar" && \
    echo "${JNDI_EXPLOIT_SHA256}  /opt/tools/JNDI-Injection-Exploit.jar" | sha256sum -c - && \
    curl -fsSL --retry 8 --retry-all-errors -o /opt/tools/stegsolve.jar \
      "https://github.com/Giotino/stegsolve/releases/download/v${STEGSOLVE_VERSION}/StegSolve-${STEGSOLVE_VERSION}.jar" && \
    echo "${STEGSOLVE_SHA256}  /opt/tools/stegsolve.jar" | sha256sum -c - && \
    printf '#!/bin/sh\nexec java -jar /opt/tools/stegsolve.jar "$@"\n' > /usr/local/bin/stegsolve && \
    chmod 0755 /usr/local/bin/stegsolve

# .NET SDK + ilspycmd for .NET reversing. The SDK tarball is pinned and SHA-512-
# verified (checksums from dotnet release-metadata) instead of piping the
# unversioned dotnet-install.sh into a root shell.
ARG DOTNET_SDK_VERSION=8.0.424
ARG DOTNET_SDK_SHA512_X64=6503fd9f464d5e3a4f43a881d2b74afc6a2c46ceda74d027f1565b7239f4b3ec884857c03c0dcd49eb52f384d5ae1fa5aaf135f0a6aabc5518103aceed643c74
ARG DOTNET_SDK_SHA512_ARM64=bb19b6779ad93d146055583d644ef269bb42501f6c7fdef51e14026cde9d5fd726d370de098a8d8504867fb24bfcb5ab88cc22bec812461aede334de1aacf7b6
ARG ILSPYCMD_VERSION=9.1.0.7988
RUN case "$TARGETARCH" in \
      amd64) dotnet_rid=x64; dotnet_sha="$DOTNET_SDK_SHA512_X64" ;; \
      arm64) dotnet_rid=arm64; dotnet_sha="$DOTNET_SDK_SHA512_ARM64" ;; \
      *) echo "unsupported .NET SDK architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    mkdir -p /usr/share/dotnet && \
    curl -fsSL --retry 8 --retry-all-errors -o /tmp/dotnet.tar.gz \
      "https://builds.dotnet.microsoft.com/dotnet/Sdk/${DOTNET_SDK_VERSION}/dotnet-sdk-${DOTNET_SDK_VERSION}-linux-${dotnet_rid}.tar.gz" && \
    echo "${dotnet_sha}  /tmp/dotnet.tar.gz" | sha512sum -c - && \
    tar -xzf /tmp/dotnet.tar.gz -C /usr/share/dotnet && \
    rm -f /tmp/dotnet.tar.gz && \
    ln -sf /usr/share/dotnet/dotnet /usr/local/bin/dotnet && \
    DOTNET_ROOT=/usr/share/dotnet /usr/local/bin/dotnet tool install --tool-path /opt/dotnet-tools \
      ilspycmd --version "$ILSPYCMD_VERSION" && \
    ln -sf /opt/dotnet-tools/ilspycmd /usr/local/bin/ilspycmd
ENV DOTNET_ROOT=/usr/share/dotnet

# 4b) SageMath via conda-forge (SageMath's officially-recommended install; the only
#     reliable route while the apt package is gone). Installed into /opt/conda; the
#     `sage` binary is symlinked onto PATH. This is a real, full sage. ~2-3G, slow.
#     conda pulls hundreds of MB; on a flaky link a single download can break mid-
#     stream ("Connection broken: IncompleteRead"). Bump conda's retry count + a
#     retry loop around the install so a transient drop doesn't void the whole layer.
# Miniforge pinned to a specific release + SHA256-verified before we execute it as
# root (supply-chain: never pipe an unverified `latest` installer into a root shell).
# Re-pin: bump MINIFORGE_VERSION, download the installer, `shasum -a 256` it.
ARG MINIFORGE_VERSION=26.3.2-3
ARG MINIFORGE_SHA256_AMD64=848194851a98903134187fbb4ab50efe87b003e0c0f808f97644b7524a62bf2c
ARG MINIFORGE_SHA256_ARM64=2c113a69297e612b01ca0f320c22a3107a11f2ab9b573d79ac868a175945ce29
RUN case "$TARGETARCH" in \
      amd64) miniforge_arch="x86_64"; miniforge_sha="$MINIFORGE_SHA256_AMD64" ;; \
      arm64) miniforge_arch="aarch64"; miniforge_sha="$MINIFORGE_SHA256_ARM64" ;; \
      *) echo "unsupported Miniforge architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL --retry 8 --retry-all-errors -o /tmp/miniforge.sh \
      "https://github.com/conda-forge/miniforge/releases/download/${MINIFORGE_VERSION}/Miniforge3-${MINIFORGE_VERSION}-Linux-${miniforge_arch}.sh" && \
    echo "${miniforge_sha}  /tmp/miniforge.sh" | sha256sum -c - && \
    bash /tmp/miniforge.sh -b -p /opt/conda && \
    rm -f /tmp/miniforge.sh && \
    /opt/conda/bin/conda config --set remote_max_retries 10 && \
    /opt/conda/bin/conda config --set remote_connect_timeout_secs 30 && \
    /opt/conda/bin/conda config --set remote_read_timeout_secs 120 && \
    for i in 1 2 3 4 5; do \
      /opt/conda/bin/conda install -y -c conda-forge sage && break || \
      { echo "conda install attempt $i failed, retrying..."; sleep 10; }; \
    done && \
    /opt/conda/bin/conda list sage | grep -qi '^sage ' && \
    /opt/conda/bin/conda clean -afy && \
    ln -sf /opt/conda/bin/sage /usr/local/bin/sage

# agent-browser requires Node.js 24+; pin the CLI runtime rather than relying
# on Kali rolling's current Node version.
ARG NODE_VERSION="24.11.1"
ARG NODE_MIRROR="https://nodejs.org/dist"
RUN case "$TARGETARCH" in \
      amd64) node_arch="x64" ;; \
      arm64) node_arch="arm64" ;; \
      *) echo "unsupported Node architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL "$NODE_MIRROR/v$NODE_VERSION/node-v$NODE_VERSION-linux-$node_arch.tar.gz" \
      -o /tmp/node.tar.gz && \
    tar -xzf /tmp/node.tar.gz -C /usr/local --strip-components=1 && \
    rm -f /tmp/node.tar.gz && \
    node --version && npm --version

# 5) Engine CLIs. claude/codex/pi via npm (global, on PATH at /usr/local/bin).
#    Credentials are injected at runtime — login state is NEVER baked.
ARG NPM_REGISTRY=""
RUN if [ -n "$NPM_REGISTRY" ]; then npm config set registry "$NPM_REGISTRY"; fi && \
    npm install -g \
      @openai/codex@0.146.0 \
      @anthropic-ai/claude-code@2.1.257 \
      @earendil-works/pi-coding-agent@0.84.1 \
      @moonshot-ai/kimi-code@0.40.1 \
      opencode-ai@1.18.22 \
      agent-browser@0.38.1

# cursor-agent is NOT an npm package — it installs via cursor.com/install into
# ~/.local/bin. Do it as the kali user so it lands in kali's HOME (the worker runs as
# kali; container_exec pins the absolute path /home/kali/.local/bin/cursor-agent).
USER kali
RUN curl -fsSL https://cursor.com/install | bash
# OMP's npm entrypoint requires Bun at runtime. Use its official prebuilt binary and
# pin the release so full/slim use the same executable.
RUN curl -fsSL https://omp.sh/install | sh -s -- --binary --ref v17.3.5 && \
    /home/kali/.local/bin/omp --version
# Grok Build is a native binary. Pin the official installer to the version used by
# the host integration so image behaviour does not change between rebuilds.
RUN curl -fsSL https://x.ai/cli/install.sh | bash -s 1.0.5 && \
    /home/kali/.grok/bin/grok --version
USER root

# Factory Droid. Pin the official binary and verify its checksum. Do not run
# the remote installer.
ARG DROID_VERSION=0.234.0
ARG DROID_SHA256_AMD64=d3a22164b64ee9d134ed7b068833ceb38c090631d7b257ae03b3b72d9e84d257
ARG DROID_SHA256_ARM64=9fb422a6733293120df8d8c34b1062d9acc997e6cac4c2740eb453f046f0161c
ARG TARGETARCH
RUN case "$TARGETARCH" in \
      amd64) droid_arch="x64"; droid_sha="$DROID_SHA256_AMD64" ;; \
      arm64) droid_arch="arm64"; droid_sha="$DROID_SHA256_ARM64" ;; \
      *) echo "unsupported Droid architecture: $TARGETARCH" >&2; exit 2 ;; \
    esac && \
    curl -fsSL --retry 8 --retry-all-errors \
      -o /tmp/droid \
      "https://downloads.factory.ai/factory-cli/releases/${DROID_VERSION}/linux/${droid_arch}/droid" && \
    echo "${droid_sha}  /tmp/droid" | sha256sum -c - && \
    install -m 0755 /tmp/droid /usr/local/bin/droid && \
    rm -f /tmp/droid && \
    droid --version

# 6) Offline knowledge base (public upstream repos only). Shallow clones, .git
#    stripped, owned by kali. ~2.6G. Domain-specific AD tooling remains excluded;
#    the general CTF pivot tool chisel is installed with the core toolchain above.
RUN mkdir -p /home/kali/knowledges /home/kali/pocs /home/kali/.local && \
    cd /home/kali/knowledges && \
      git clone --depth=1 https://github.com/swisskyrepo/PayloadsAllTheThings.git && \
      git clone --depth=1 https://github.com/swisskyrepo/InternalAllTheThings.git && \
      git clone --depth=1 https://github.com/HackTricks-wiki/hacktricks.git && \
      git clone --depth=1 https://github.com/HackTricks-wiki/hacktricks-cloud.git && \
    cd /home/kali/pocs && \
      git clone --depth=1 https://github.com/vulhub/vulhub.git && \
      git clone --depth=1 https://github.com/Threekiii/Awesome-POC.git && \
    git clone --depth=1 https://github.com/projectdiscovery/nuclei-templates.git \
      /home/kali/.local/nuclei-templates && \
    find /home/kali/knowledges /home/kali/pocs /home/kali/.local/nuclei-templates \
      -name .git -type d -prune -exec rm -rf {} + && \
    chown -R kali:kali /home/kali/knowledges /home/kali/pocs /home/kali/.local

# Target-side chisel assets. The Kali package remains the local client at
# /usr/bin/chisel; these pinned static binaries are uploaded only after a Worker
# has dynamically identified the controlled target architecture.
ARG TARGET_CHISEL_VERSION=1.12.0
ARG TARGET_CHISEL_SHA256_AMD64=f3f180f1d93aa72cce4e6386f98cc06569a0146fbd65eb4423cf83e6434bcfe6
ARG TARGET_CHISEL_SHA256_ARM64=2ec6152cd2c74fe0146d4d79e4e7aa174521368c56e433d55e023a92ea404ec3
RUN mkdir -p /usr/share/chisel-common-binaries && \
    curl -fsSL --retry 8 --retry-all-errors \
      -o /tmp/chisel-linux-amd64.gz \
      "https://github.com/jpillora/chisel/releases/download/v${TARGET_CHISEL_VERSION}/chisel_${TARGET_CHISEL_VERSION}_linux_amd64.gz" && \
    curl -fsSL --retry 8 --retry-all-errors \
      -o /tmp/chisel-linux-arm64.gz \
      "https://github.com/jpillora/chisel/releases/download/v${TARGET_CHISEL_VERSION}/chisel_${TARGET_CHISEL_VERSION}_linux_arm64.gz" && \
    echo "${TARGET_CHISEL_SHA256_AMD64}  /tmp/chisel-linux-amd64.gz" | sha256sum -c - && \
    echo "${TARGET_CHISEL_SHA256_ARM64}  /tmp/chisel-linux-arm64.gz" | sha256sum -c - && \
    gzip -dc /tmp/chisel-linux-amd64.gz \
      > /usr/share/chisel-common-binaries/chisel-linux-amd64 && \
    gzip -dc /tmp/chisel-linux-arm64.gz \
      > /usr/share/chisel-common-binaries/chisel-linux-arm64 && \
    chmod 0755 /usr/share/chisel-common-binaries/chisel-linux-* && \
    file /usr/share/chisel-common-binaries/chisel-linux-amd64 \
      | grep -Eq 'x86-64.*statically linked' && \
    file /usr/share/chisel-common-binaries/chisel-linux-arm64 \
      | grep -Eq 'ARM aarch64.*statically linked' && \
    /usr/bin/chisel --version && \
    rm -f /tmp/chisel-linux-amd64.gz /tmp/chisel-linux-arm64.gz

# 6b) File capabilities must fit Docker's default bounding set. Kali's nmap ships
#     /usr/lib/nmap/nmap with cap_net_bind_service,cap_net_admin,cap_net_raw=eip;
#     cap_net_admin is NOT in Docker's default set, so execve fails with EPERM
#     ("Operation not permitted") for kali and root alike unless the container adds
#     NET_ADMIN — which production only does for VPN runs (container_exec.py).
#     Narrow nmap to raw/bind (both default caps), then sweep any other file caps
#     down to the Docker default set so a future Kali package cannot reintroduce it.
RUN set -eu; \
    if ! command -v setcap >/dev/null 2>&1; then \
      apt-get update && apt-get install -y --no-install-recommends libcap2-bin && \
      apt-get clean && rm -rf /var/lib/apt/lists/*; \
    fi; \
    setcap cap_net_raw,cap_net_bind_service+eip /usr/lib/nmap/nmap; \
    allowed=" cap_chown cap_dac_override cap_fowner cap_fsetid cap_kill cap_setgid cap_setuid cap_setpcap cap_net_bind_service cap_net_raw cap_sys_chroot cap_mknod cap_audit_write cap_setfcap "; \
    getcap -r /usr /opt /home/kali 2>/dev/null | while read -r path spec; do \
      case "$spec" in *" "*) echo "unsupported file caps spec on $path: $spec" >&2; exit 1 ;; esac; \
      caps="${spec%%[=+]*}"; flags="${spec#"$caps"}"; keep=""; \
      for c in $(echo "$caps" | tr ',' ' '); do \
        case "$allowed" in *" $c "*) keep="${keep:+$keep,}$c" ;; esac; \
      done; \
      if [ -n "$caps" ] && [ "$keep" = "$caps" ]; then continue; fi; \
      if [ -n "$keep" ]; then setcap "${keep}${flags}" "$path"; else setcap -r "$path"; fi; \
      echo "narrowed file caps: $path $spec -> ${keep:+$keep$flags}"; \
    done; \
    getcap -r /usr /opt /home/kali 2>/dev/null

# 7) Runtime Agent (Go supervisor, §10) — cross-compiled static binary, COPY'd in.
#    build.sh and release-images.yml name it by the target architecture.
COPY ./runtime_agent-${TARGETARCH} /opt/muteki/runtime_agent
COPY ./offline_acp_bridge.py /opt/muteki/offline_acp_bridge.py
COPY ./omp_offline_config.yml /opt/muteki/omp_offline_config.yml
COPY ./kimi_offline_agent.md /opt/muteki/kimi_offline_agent.md
COPY ./grok_offline_agent.md /opt/muteki/grok_offline_agent.md
RUN chmod +x /opt/muteki/runtime_agent

# 8) Tool-awareness map, baked OUTSIDE the mounted workspace (坑 A). The supervisor
#    cp's these into /home/kali/workspace AFTER the host bind-mount, so the CLIs
#    auto-read them (claude←CLAUDE.md, codex←AGENTS.md). Same content, two names.
COPY ./AGENTS.md /opt/muteki/AGENTS.md
COPY ./AGENTS.md /opt/muteki/CLAUDE.md
COPY ./blackboard.SKILL.md /opt/muteki/muteki-blackboard/SKILL.md
COPY ./blackboard.py /opt/muteki/muteki-blackboard/blackboard.py
RUN chmod +x /opt/muteki/muteki-blackboard/blackboard.py && \
    cp /opt/muteki/muteki-blackboard/blackboard.py /usr/local/bin/blackboard.py && \
    chmod +x /usr/local/bin/blackboard.py

# 9) workspace + control dir; supervisor as ENTRYPOINT (PID1, replaces sleep infinity).
RUN mkdir -p /home/kali/workspace && chown kali:kali /home/kali/workspace && \
    mkdir -p /run/muteki/control /run/muteki/accounts && \
    chown -R kali:kali /run/muteki

# OCI version/revision labels so deployment and the settings "worker image check"
# (P2-v3) can compare the pulled image against the expected release identity.
# release-images.yml passes IMAGE_VERSION + IMAGE_REVISION=$GITHUB_SHA.
ARG IMAGE_VERSION=dev
ARG IMAGE_REVISION=unknown
LABEL org.opencontainers.image.title="muteki-worker" \
      org.opencontainers.image.description="Kali muteki worker: full CTF toolchain + reverse-connector + container agent CLIs" \
      org.opencontainers.image.version="${IMAGE_VERSION}" \
      org.opencontainers.image.revision="${IMAGE_REVISION}"

WORKDIR /home/kali/workspace
# The separately installed Python tools can shadow Kali's cryptography package
# with an arm64 wheel that raises SIGILL during sqlmap startup under Docker
# Desktop. Restore the distro build after all pip layers; it is CPU compatible
# and provides package metadata for downstream Python tools.
RUN python3 -c 'from pathlib import Path; import shutil, sysconfig; p=Path(sysconfig.get_paths()["purelib"]); assert p.is_relative_to("/usr/local/lib"); targets=[p/"cryptography", *p.glob("cryptography-*.dist-info")]; [shutil.rmtree(x) for x in targets if x.is_dir()]' && \
    python3 -c 'import cryptography, cryptography.hazmat.bindings._rust; from pathlib import Path; assert Path(cryptography.__file__).is_relative_to("/usr/lib/python3/dist-packages")' && \
    sqlmap --version
# Supervisor runs as root (PID1) so it can drop workers to the kali uid; the worker
# itself runs as kali (with sudo). It listens on the bind-mounted control socket.
ENTRYPOINT ["/opt/muteki/runtime_agent", "--sock", "/run/muteki/control/control.sock", "--workspace", "/home/kali/workspace"]
