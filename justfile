set dotenv-load

default:
    @just --list

# Run an example. Extra arguments go to the example.
[positional-arguments]
run example="demo" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    example="$1"
    shift
    cargo run --frozen --example "$example" -- "$@"

# Run unit, integration, and documentation tests. Extra arguments go to Cargo.
[positional-arguments]
test *args:
    cargo test --frozen "$@"

# Run tests with the minimum supported Rust version.
test-min:
    nix develop .#rust175 --command just test

build:
    cargo build --frozen --all-targets

# Check Rust code with the toolchain in the current shell.
check-rust: build lint-rust test

lint-rust:
    cargo clippy --frozen --all-targets --all-features -- -D warnings

lint: lint-rust
    deadnix --fail flake.nix
    statix check .
    markdownlint README.md

typos:
    typos

fmt-check:
    cargo fmt --all -- --check
    nixfmt --check flake.nix
    just --fmt --check

fmt:
    cargo fmt --all
    nixfmt flake.nix
    just --fmt

# Run all hooks, including Rust checks and tests.
check: hooks

hooks:
    pre-commit run --all-files

# Check stable Rust, Rust 1.75, and all hooks in the Nix build sandbox.
check-nix:
    nix flake check

doc:
    cargo doc --frozen --no-deps

# Create a commit with a Conventional Commits message.
commit:
    cz commit
