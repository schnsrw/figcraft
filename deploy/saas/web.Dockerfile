# Build from the repository root:
#   docker build -f deploy/saas/web.Dockerfile --build-arg CADDY_IMAGE=<reviewed-ref> -t figcraft-web:local .
ARG CADDY_IMAGE=caddy:2.10.2-alpine

FROM node:22.18.0-bookworm-slim AS build
WORKDIR /src
COPY webapp/ ./webapp/
RUN npm ci --prefix webapp && npm run build --prefix webapp

FROM ${CADDY_IMAGE}
COPY deploy/saas/web.Caddyfile /etc/caddy/Caddyfile
COPY --from=build /src/webapp/saas/dist/ /srv/
EXPOSE 8080
