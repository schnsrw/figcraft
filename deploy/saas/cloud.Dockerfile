# Build from the repository root:
#   docker build -f deploy/saas/cloud.Dockerfile -t figcraft-cloud:dev .

FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p figcraft-cloud

# Distroless runtime has no shell or package manager; the named nonroot user is UID 65532.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/figcraft-cloud /usr/local/bin/figcraft-cloud
USER nonroot
EXPOSE 8081
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/figcraft-cloud"]
