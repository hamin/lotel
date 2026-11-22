# Install

lotel is one binary for macOS and Linux, on arm64 or x86_64. The DuckDB extensions it needs are built in, so there is nothing else to install. Pick one of the ways below, then go to the [Quickstart](quickstart.md).

## Install script

```sh
curl -fsSL https://lotel.io/install.sh | sh
```

The script downloads the latest release for your platform from GitHub, checks it against the release's `SHA256SUMS.txt`, and puts `lotel` in `~/.local/bin`. If that directory is not writable it falls back to `/usr/local/bin` with `sudo`.

To pick the version or the directory:

```sh
curl -fsSL https://lotel.io/install.sh | sh -s -- 0.1.0 --prefix ~/bin
```

`--prefix` never uses `sudo`: if the directory is not writable, the script stops.

## Homebrew

```sh
brew install hamin/tap/lotel
```

To run lotel in the background and start it at login:

```sh
brew services start lotel
```

The service keeps its data in `$(brew --prefix)/var/lotel` and logs to `$(brew --prefix)/var/log/lotel.log`. Run `lotel` in a terminal once first if you want to see the startup banner with the token, or read it from the log.

## Cargo

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), cargo downloads the release binary instead of compiling:

```sh
cargo binstall lotel
```

To build from source with cargo:

```sh
cargo install lotel
```

This compiles DuckDB, so it takes several minutes and needs a C++ compiler. The build downloads the pinned DuckDB extensions with `curl` and checks each one's sha256 before building them in.

## Docker

There is no published image yet. Build one from a checkout:

```sh
git clone https://github.com/hamin/lotel && cd lotel
docker build -t lotel .
docker run -p 8080:8080 -p 4318:4318 -v "$PWD/lotel-data:/data" lotel
```

Inside the container lotel listens on all interfaces and keeps its data in `/data`.

## From a checkout

You need Rust and [just](https://github.com/casey/just).

```sh
git clone https://github.com/hamin/lotel && cd lotel
just build                 # fetches the pinned extensions, then cargo build --release
./target/release/lotel
```

## Check the install

```sh
lotel --version
```

This prints the version, the commit it was built from, and the DuckDB and capped versions.

## Upgrade and uninstall

Upgrade the same way you installed: run the install script again, `brew upgrade lotel`, or `cargo binstall lotel` / `cargo install lotel`. Your data directory is kept.

To uninstall, delete the binary (`rm ~/.local/bin/lotel`, `brew uninstall lotel`, or `cargo uninstall lotel`) and, if you want, the data directory.

## Supported platforms

| Platform | Release file |
| --- | --- |
| macOS, Apple silicon | `lotel-<version>-darwin-arm64.tar.gz` |
| macOS, Intel | `lotel-<version>-darwin-x86_64.tar.gz` |
| Linux x86_64, glibc 2.35 or newer | `lotel-<version>-linux-x86_64.tar.gz` |
| Linux arm64, glibc 2.35 or newer | `lotel-<version>-linux-aarch64.tar.gz` |

Windows and musl-based Linux (Alpine) are not supported yet.
