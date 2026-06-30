# ---- build ----
FROM rust:1-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --bin linkanalyzer

# ---- runtime ----
FROM debian:bookworm-slim
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
