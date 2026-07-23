# syntax=docker/dockerfile:1.7

ARG RUST_VERSION=1.97.1
ARG MDBOOK_VERSION=0.5.3

FROM rust:${RUST_VERSION}-slim-bookworm AS builder
ARG MDBOOK_VERSION
RUN cargo install mdbook --version "${MDBOOK_VERSION}" --locked

WORKDIR /workspace
COPY book.toml ./
COPY docs ./docs
RUN mdbook build

FROM nginxinc/nginx-unprivileged:1.30.4-alpine-slim
COPY deploy/docs/nginx.conf /etc/nginx/conf.d/default.conf
COPY --from=builder --chown=101:101 /workspace/target/wookie-docs/ /usr/share/nginx/html/

USER 101:101
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD wget -q -O /dev/null http://127.0.0.1:8080/healthz || exit 1
