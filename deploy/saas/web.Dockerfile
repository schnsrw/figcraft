# Build from the repository root:
#   docker build -f deploy/saas/web.Dockerfile --build-arg CADDY_IMAGE=<reviewed-ref> -t figcraft-web:local .
#
# One static image serves the SaaS web app at `/` and the FigCraft editor (apps/figcraft-web,
# Rust compiled to wasm32) at `/edit/`. The editor stage pins trunk; trunk fetches the
# wasm-bindgen CLI matching Cargo.lock and the binaryen (`wasm-opt`) release pinned in
# apps/figcraft-web/Trunk.toml.
ARG CADDY_IMAGE=caddy:2.10.2-alpine

FROM node:22.18.0-bookworm-slim AS build
WORKDIR /src
COPY webapp/ ./webapp/
RUN npm ci --prefix webapp && npm run build --prefix webapp

FROM rust:1.95-bookworm AS editor
ARG TRUNK_VERSION=0.21.14
RUN rustup target add wasm32-unknown-unknown
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked trunk --version "${TRUNK_VERSION}"
WORKDIR /src
COPY . .
# No craft-fonts checkout in the build context: the editor uses egui's bundled fonts.
ENV CRAFT_FONTS_DIR=/nonexistent
# Trunk keeps the wasm-bindgen and wasm-opt releases it downloads under $XDG_CACHE_HOME/trunk; a
# cache mount keeps them across builds so only the first build fetches them from GitHub.
ENV XDG_CACHE_HOME=/trunk-cache
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/trunk-cache \
    cd apps/figcraft-web \
    && trunk build --release --locked --public-url /edit/ --dist /out/edit \
    && test -s /out/edit/index.html \
    && ls /out/edit/*.wasm >/dev/null

FROM ${CADDY_IMAGE}
COPY deploy/saas/web.Caddyfile /etc/caddy/Caddyfile
# Rendered per request from FIGCRAFT_BRAND_* (docs/saas/branding-and-configuration.md).
COPY deploy/saas/branding.json.tmpl /etc/caddy/branding/branding.json
COPY --from=build /src/webapp/saas/dist/ /srv/
COPY --from=editor /out/edit/ /srv/edit/
EXPOSE 8080
