# tacitus-billing — build from the workspace root:
#   docker build -t tacitus-billing -f crates/tacitus-billing/Dockerfile .
FROM rust:1-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p tacitus-billing

FROM debian:bookworm-slim
RUN useradd --system --home /data billing
COPY --from=build /src/target/release/tacitus-billing /usr/local/bin/tacitus-billing
# /data is the SAME volume the relay mounts — the two services share nothing
# but entitlements.json. STRIPE_* secrets are injected at `docker run -e`,
# never baked into the image.
ENV TACITUS_BILLING_BIND=0.0.0.0:8092 \
    TACITUS_BILLING_ENTITLEMENTS=/data/entitlements.json
VOLUME /data
EXPOSE 8092
USER billing
CMD ["tacitus-billing"]
