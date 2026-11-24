# Upstream, built from a pinned commit, used as the reference for
# comparing decoder output (see tools/compare.sh). No SDR support is needed: it only
# reads IQ files and network streams here.
FROM debian:trixie-slim AS build
ARG READSB_REF=f815584d9f03ea5f532f02476fbb072fa20251a2
RUN apt-get update && apt-get -y install --no-install-recommends \
        ca-certificates git gcc make libc6-dev libncurses-dev zlib1g-dev libzstd-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN git init -q . \
    && git fetch -q --depth 1 https://github.com/wiedehopf/readsb "$READSB_REF" \
    && git checkout -q FETCH_HEAD \
    && make -j"$(nproc)" RTLSDR=no OPTIMIZE="-O2" \
    && install -m 0755 readsb /usr/local/bin/readsb

FROM debian:trixie-slim
RUN apt-get update && apt-get -y install --no-install-recommends libncurses6 zlib1g libzstd1 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/readsb /usr/local/bin/readsb
ENTRYPOINT ["/usr/local/bin/readsb"]
