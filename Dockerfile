# ---- Clio: container "tot en un" (multietapa) ----
#
# Un sol Dockerfile construeix DÜES imatges des d'aquest repo (cap dependència
# d'imatges base pròpies ja publicades):
#
#   docker build --target app -t <reg>/clio:latest .
#       -> imatge LLEUGERA: només clio serve (API + web). És l'equivalente
#          exacte de l'antic Dockerfile. Útil per al desplegament web.
#
#   docker build --target stream -t <reg>/clio-stream:latest .
#   docker build .                              (mateix; stream és el target per defecte)
#       -> imatge "TOT EN UN": clio serve + emissió headless a Twitch
#          (Xvfb + Chromium + ffmpeg) + tota la CLI. Tria el mode amb:
#            CLIO_MODE=serve   (per defecte)  CLIO_MODE=stream
#            o passa un subordre CLI directe: docker run img images --limit 5

# ---- build ----
# trixie (glibc 2.38): el binari precompilat d'onnxruntime (via fastembed/ort)
# referencia símbols __isoc23_* que bookworm (glibc 2.36) no té.
FROM rust:1-trixie AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --bin linkanalyzer

# ---- app: runtime lleuger (clio serve) ----
FROM debian:trixie-slim AS app
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git libstdc++6 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
# public/ no es copia: serve el regenera a l'arrencada (HTML/CSS/JS incrustats al binari).
COPY --from=build /app/target/release/linkanalyzer /usr/local/bin/linkanalyzer
RUN mkdir -p data public
ENV BIND_ADDR=0.0.0.0:8080 \
    DATABASE_URL=sqlite://data/linkanalyzer.db \
    PUBLIC_DIR=public
EXPOSE 8080
CMD ["linkanalyzer", "serve"]

# ---- stream: "tot en un" (a partir de app) ----
FROM app AS stream
# Drivers VA-API per a codificar amb GPU:
#   mesa-va-drivers        -> AMD (radeonsi/VCN)
#   intel-media-va-driver  -> Intel Gen8+ / Kaby Lake (iHD); el driver que cal
#                             per a Gen9 i posteriors (H.264, HEVC/VP9 decode).
# Amb tots dos, libva tria el driver segons el node /dev/dri de la màquina
# (AMD -> radeonsi, Intel -> iHD). Si es vol forçar: LIBVA_DRIVER_NAME=iHD.
RUN apt-get update && apt-get install -y --no-install-recommends \
        xvfb chromium xdotool ffmpeg curl \
        libva2 libva-drm2 mesa-va-drivers intel-media-va-driver vainfo \
    && rm -rf /var/lib/apt/lists/*
COPY scripts/stream.sh /usr/local/bin/stream.sh
RUN chmod +x /usr/local/bin/stream.sh
# Dispatcher: CLIO_MODE=stream -> emissió; sinó -> linkanalyzer serve (o la
# subordre CLI passada per arguments).
COPY scripts/clio-entry.sh /usr/local/bin/clio
RUN chmod +x /usr/local/bin/clio
ENTRYPOINT ["clio"]
# Important: anul·la el CMD heretat de `app` ("linkanalyzer serve"). Amb
# ENTRYPOINT=clio, el CMD quedaria duplicat; en blanc deixa que el dispatcher
# decideixi: CLIO_MODE=stream -> stream.sh; sense res -> linkanalyzer serve;
# amb arguments -> la subordre CLI que passis.
CMD []

# El target per defecte de `docker build .` és stream (l'última etapa), és a
# dir la imatge "tot en un". Usa --target app per a la lleugera.
