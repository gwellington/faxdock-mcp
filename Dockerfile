FROM rust:1.96-alpine AS source

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

FROM source AS test
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo test --locked --release

FROM source AS build
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --locked --release && cp target/release/telnyx-fax /usr/local/bin/telnyx-fax

FROM alpine:3.23
RUN addgroup -S -g 1000 fax && adduser -S -D -H -u 1000 -G fax fax
COPY --from=build /usr/local/bin/telnyx-fax /usr/local/bin/telnyx-fax
USER fax

CMD ["sleep", "infinity"]
