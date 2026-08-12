# mepul

Profile-driven image delivery for ephemeral CI.

CI runners start clean, so every job pulls and unpacks its image from scratch;
nothing carries over from the run before. But the same pipeline runs over and
over, and each run touches only a small part of the image it was given. `mepul`
watches what a job actually opens, keeps that list and those file contents
outside the runner, and assembles the next run's root filesystem out of them
instead of pulling.

```text
run 1   pull the image, run the job, observe it with eBPF
        -> remember which files were touched, and their contents
run 2   assemble a root filesystem from what was remembered
        -> no registry, no layer download, no unpacking
```

The second run reaches a working root filesystem without contacting the registry
at all, and places only the files the first run was observed to touch rather
than everything the image ships.

## Requirements

- Linux with eBPF and overlayfs (any modern kernel)
- Rust / Cargo
- `clang` — the eBPF observer is compiled at build time and embedded in the
  binary, so nothing has to be installed on the runner itself
- `root` for `mepul run`, which mounts filesystems and loads eBPF programs
- Docker is needed only for `mepul pull`, which loads an image into Docker's
  image store. `mepul run` does not use Docker at all.

On Debian/Ubuntu:

```bash
sudo apt-get install -y clang
```

## Build

```bash
cargo build --release
```

## Usage

### Run a job on a disposable root filesystem

```bash
sudo ./target/release/mepul run ubuntu:24.04 --pipeline build -- \
  /bin/bash -c 'cat /etc/os-release'
```

The first run for a pipeline pulls the image and learns; run the same command
again and it is served from the cache:

```text
warm: placed 23 entr(ies) from the cache, no registry access
mode:     warm
```

`--pipeline` is the identity the profile is stored under. Two runs only share a
profile if they share a pipeline name, so it should name the job, not the
machine.

Options:

| flag | effect |
| --- | --- |
| `--cold` | Ignore the profile and pull, without discarding what has been learned. |
| `--no-trace` | Skip observation. The profile stops improving. |
| `--verbose`, `-v` | Report every observed path, what became of it, and what the run added to the profile and the cache. |

`--verbose` is the way to see the mechanism working:

```text
  added      /bin/true
  outside    /etc/ld.so.preload
  added      /etc/ld.so.cache
  added      /lib/aarch64-linux-gnu/libc.so.6
--- profile gained 12 entr(ies)
  file  0644  /etc/ld.so.cache  sha256:18b6a932cf34… 5.1 KiB (new)
  file  0755  /usr/bin/true  sha256:69851c424c45… 66.2 KiB (new)
  file  0755  /usr/lib/…/ld-linux-aarch64.so.1  sha256:49005ef8e9db… 199.2 KiB (new)
  file  0755  /usr/lib/…/libc.so.6  sha256:6e3cc56b9888… 1682.5 KiB (new)
--- cache gained 4 entr(ies), 1953.0 KiB
```

Three observed paths become twelve profile entries: the directories and symlinks
along the way, plus the loader. `outside` is host activity the tracer picked up.
Run it again against a populated cache and the same files report
`deduplicated`, with the cache gaining nothing.

### Inspect what a pipeline has learned

```bash
./target/release/mepul profile build
./target/release/mepul profile build --paths
```

### Pull an image into Docker

Unrelated to the above — this is a plain OCI puller that ends at
`docker images`.

```bash
sudo ./target/release/mepul pull hello-world:latest
docker images
```

Pass `--sock` if the daemon is not on `/var/run/docker.sock`.

## How it works

Four pieces, split by what survives a run:

| | | lives |
| --- | --- | --- |
| observer | reports the paths a job reaches for, and nothing else | in the runner |
| disposable rootfs | overlay with a tmpfs upper; discarded with the job | in the runner |
| profile | which paths this pipeline uses, and their digests | outside |
| file cache | the contents, addressed by their own hash | outside |

The observer only ever carries path strings. Contents are read from the job's
own root filesystem afterwards, hashed, and stored under that hash — so two
pipelines that use the same file store it once.

Two things are recorded that no probe ever reports. Symlinks are followed, and
every directory along the way is kept, because a cached `/usr/bin/dash` is
useless if `/bin/sh` cannot resolve to it. And a binary's ELF interpreter is read
out of its `PT_INTERP` header, because the kernel opens the loader inside
`execve` without a syscall of its own — the one file without which nothing runs
is the one file tracing cannot see.

Observation starts only once the job has called `chroot`. Getting a job into its
root filesystem takes host binaries running under the pid the job will use, and
they open their own libraries first; on a runner whose distribution matches the
image, those paths resolve on both sides and would be recorded as if the job
needed them.

### State

```text
~/.cache/mepul/
├── profiles/<pipeline>.json   which files this pipeline uses
├── files/sha256/<hash>        their contents
└── blobs/sha256/<hash>        layer blobs, so a later cold run skips the download
```

`$MEPUL_CACHE_DIR` overrides the root. Note that `sudo` changes `HOME`, so set it
explicitly when it matters:

```bash
sudo MEPUL_CACHE_DIR="$HOME/.cache/mepul" ./target/release/mepul run …
```

A warm run reads only `profiles/` and `files/`. `blobs/` is the one part of the
cache it never touches.

## Development

```bash
cargo test
```

The eBPF observer lives in `bpf/observe.bpf.c` and is compiled by `build.rs`.
`bpf/vmlinux.h` is a hand-written minimum rather than a BTF dump, which is what
lets the program build with clang alone — no `bpftool`, no CO-RE toolchain, and
nothing to regenerate per kernel.

## Status

A proof of concept. Known limits, all reproducible:

- A profile is replayed in full before the job starts, so a job that takes a
  different path than it did last time may find a file missing. That is only
  detected through the job's exit code, so a run that degrades without failing
  passes silently.
- Falling back rebuilds the profile from scratch instead of merging, so a
  pipeline that alternates between two paths never converges.
- Files the job itself writes are recorded, and reappear in the next run's lower
  layer.
- Profiles are keyed by image tag, not by manifest digest, so a tag that moves to
  a new image is not noticed.
- `mepul run` isolates the mount namespace only. It is not a container runtime.
- Blob downloads are not retried, so one reset connection fails the run.
