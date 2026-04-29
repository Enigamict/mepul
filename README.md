# mepul

`mepul` is a small Rust image puller. It fetches an image from an OCI/Docker
image with Docker so it appears in `docker images`.

The current flow is:

```text
registry
  -> temporary blob store
  -> temporary image record
  -> OCI archive stream
  -> Docker Engine API /images/load
  -> docker images
```

## Requirements

- Rust / Cargo
- Docker daemon
- Access to `/var/run/docker.sock`

## Usage

Build the binary:

```bash
cargo build --release
```

Pull an image:

```bash
./target/release/mepul hello-world:latest
```
or

```bash
cargo run -- hello-world:latest
```

```bash
docker images
```



