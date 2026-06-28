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
# distroless/cc gives us glibc + CA certificates (needed for the HTTPS calls to
# R2, CouchDB and Cloudflare) and almost nothing else.
FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/allpackages /usr/local/bin/allpackages
ENTRYPOINT ["allpackages"]
