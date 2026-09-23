# den-mcp — a static musl binary built on Alpine, copied into `scratch`, like every den Rust service without a
# helper binary. It speaks plain HTTP to atlas on the LAN and never opens TLS itself, so the image carries no CA
# store and no libc.

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Cache deps: build against the manifests and a dummy main first, so a code-only change re-runs only the final
# (LTO'd) link of this crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
# rust:alpine's default host target is x86_64-unknown-linux-musl → a fully static binary; `strip` is in the profile.
RUN touch src/main.rs && cargo build --release --locked

FROM scratch AS runtime
COPY --from=build /src/target/release/den-mcp /den-mcp
ENV PORT=8096
EXPOSE 8096
# scratch has no /etc/passwd, so the uid is numeric: 65532, the one every den image runs as. No HEALTHCHECK:
# den-update probes /health when it deploys, and a periodic probe keeps an idle box awake.
USER 65532:65532
ENTRYPOINT ["/den-mcp"]
