# Build and self-host these docs

The Wookie documentation is an mdBook that builds into portable static HTML.
Serve the generated directory with any ordinary web server, or use the included
unprivileged container.

> [!important]
> This publishes the Wookie product documentation only. The image contains no
> `~/.wookie` data, has no API or daemon, and cannot read live wiki pages,
> sessions, notifications, credentials, or project files.

## Preview locally

Install the pinned documentation builder:

```sh
cargo install mdbook --version 0.5.3 --locked
```

Start a local server with live reload:

```sh
mdbook serve --hostname 127.0.0.1 --port 3000
```

Open `http://127.0.0.1:3000`.

## Build portable static files

```sh
mdbook build
```

The complete site is written to `target/wookie-docs/`. Copy that directory to
S3-compatible object storage, a static hosting service, Nginx, Caddy, Apache,
or any server that can return static files.

For a quick local check without mdBook's live-reload server:

```sh
python3 -m http.server 8080 --bind 127.0.0.1 --directory target/wookie-docs
```

## Run the included container

Docker Compose builds the book and serves it from an unprivileged Nginx process:

```sh
docker compose -f compose.docs.yml up --build -d --wait
```

The default address is `http://localhost:8080`. Choose another host port
without editing the Compose file:

```sh
WOOKIE_DOCS_PORT=9090 docker compose -f compose.docs.yml up --build -d --wait
```

The default bind address is loopback-only. Set `WOOKIE_DOCS_BIND=0.0.0.0`
only when the host firewall and network policy are ready to expose the port.

Verify health and stop it:

```sh
curl --fail http://localhost:8080/healthz
docker compose -f compose.docs.yml down
```

The Compose service drops Linux capabilities, enables `no-new-privileges`,
limits process count, uses a read-only root filesystem, and provides only the
small temporary directory Nginx needs.

## Put it behind a reverse proxy

The included image listens on port `8080` and expects to be mounted at the root
of a hostname, for example `https://docs.example.com/`. Terminate TLS in your
existing reverse proxy or ingress and forward to the container.

If the site must live below a path such as `/wookie/`, either strip that prefix
before proxying or set the appropriate mdBook `site-url` in `book.toml` and
rebuild. Test search, navigation, and static assets from the final public URL.

## Update a deployment

Build from a reviewed Wookie commit or release tag so the documentation and
its commands stay aligned:

```sh
git checkout <reviewed-tag-or-commit>
docker compose -f compose.docs.yml build --pull
docker compose -f compose.docs.yml up -d
```

`mdbook build` is part of CI. A documentation change that cannot produce the
static site should not be merged.

## Operate it safely

- Do not mount `WOOKIE_HOME`, a source checkout, SSH material, or cloud
  credentials into the documentation container.
- Treat every file below `docs/` as public build input. Do not store secrets,
  private notes, or unpublished artifacts there.
- Keep authentication and network policy in the reverse proxy if the
  documentation should be private.
- Rebuild after Wookie command or configuration changes so examples do not
  drift.
- Treat the generated directory as disposable output; edit the Markdown under
  `docs/`, not files below `target/wookie-docs/`.
