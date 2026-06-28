# --- build stage ---------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src

# Cache dependency builds: copy manifests first, build a dummy, then the source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release || true
COPY src ./src
# Touch so cargo recompiles the real main.rs over the dummy.
RUN touch src/main.rs && cargo build --release

# --- runtime stage -------------------------------------------------------
# debian-slim rather than distroless: GitHub Actions `container:` jobs start the
# image with a keep-alive command (`tail -f /dev/null`) and exec steps via a
# shell, so the runtime image must provide coreutils + sh. ca-certificates is
# needed for the HTTPS calls to R2, CouchDB and Cloudflare.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/allpackages /usr/local/bin/allpackages
ENTRYPOINT ["allpackages"]
