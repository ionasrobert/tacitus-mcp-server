# tacitus-relay — build from the workspace root:
#   docker build -t tacitus-relay -f crates/tacitus-relay/Dockerfile .
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p tacitus-relay

FROM debian:bookworm-slim
RUN useradd --system --home /data relay
COPY --from=build /src/target/release/tacitus-relay /usr/local/bin/tacitus-relay
ENV TACITUS_RELAY_BIND=0.0.0.0:8091 \
    TACITUS_RELAY_DATA=/data
VOLUME /data
EXPOSE 8091
USER relay
CMD ["tacitus-relay"]
