# den-mcp — a static musl binary built on Alpine, copied into `scratch`, like every den Rust service without a
# helper binary. It speaks plain HTTP to atlas on the LAN and never opens TLS itself, so the image carries no CA
# store and no libc.

FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Cache deps: build against the manifests and a dummy main first, so a code-only change re-runs only the final
# (LTO'd) link of this crate. Cargo.toml's version is fixed (the release version lives in VERSION and reaches the
# image as DEN_VERSION below), so a release commit leaves this layer cached.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
# rust:alpine's default host target is x86_64-unknown-linux-musl → a fully static binary; `strip` is in the profile.
RUN touch src/main.rs && cargo build --release --locked

# The release marker is copied into the runtime image, so a missing or malformed version fails the build before an
# image can be pushed. It only touches the final image, never the Rust build above.
FROM alpine:3 AS release-metadata
ARG DEN_VERSION
RUN printf '%s\n' "$DEN_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
    || { echo "DEN_VERSION must be X.Y.Z, got '$DEN_VERSION'" >&2; exit 1; }; \
    printf '%s' "$DEN_VERSION" > /den-version

FROM scratch AS runtime
ARG DEN_VERSION
ARG GIT_REVISION
ENV PORT=8096 \
    DEN_VERSION=$DEN_VERSION
LABEL org.opencontainers.image.version=$DEN_VERSION \
      org.opencontainers.image.revision=$GIT_REVISION
COPY --from=release-metadata /den-version /den-version
COPY --from=build /src/target/release/den-mcp /den-mcp
EXPOSE 8096
# scratch has no /etc/passwd, so the uid is numeric: 65532, the one every den image runs as. No HEALTHCHECK:
# den-update probes /health when it deploys, and a periodic probe keeps an idle box awake.
USER 65532:65532
ENTRYPOINT ["/den-mcp"]
