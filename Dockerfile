# The multiplayer server (src/server). Build from the repo root:
#   docker build -t flight-sim-server .
#   docker run -p 8080:8080 -e FLIGHT_SIM_DEV_AUTH=1 -v flight-sim-data:/data flight-sim-server
# The leaderboard is a SQLite file at FLIGHT_SIM_DB (/data/flight_sim.db): mount a
# volume at /data so it survives redeploys. (rusqlite builds SQLite itself with the
# image's C compiler, so the runtime needs no library.)
FROM rust:1-slim-bookworm AS build
WORKDIR /app
# dependencies first (cached until Cargo.toml / Cargo.lock change)
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src/server && echo 'fn main() {}' > src/main.rs && echo 'fn main() {}' > src/server/main.rs \
    && cargo build --release --locked --no-default-features --features server --bin flight_sim_server \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs src/server/main.rs \
    && cargo build --release --locked --no-default-features --features server --bin flight_sim_server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home server \
    && mkdir -p /data && chown server:server /data
COPY --from=build /app/target/release/flight_sim_server /usr/local/bin/flight_sim_server
USER server
ENV PORT=8080 FLIGHT_SIM_DB=/data/flight_sim.db
EXPOSE 8080
CMD ["flight_sim_server"]
