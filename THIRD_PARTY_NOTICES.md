# Third-party notices

`mcp-agent` is distributed under the Apache License 2.0 and includes or adapts
third-party open-source software.

## OpenAI Codex

This distribution contains source derived from OpenAI Codex commit
`8cabf5a6cf103cebe338d46346e43e3201e64f41`, licensed under Apache-2.0.
The retained upstream license and notice are packaged as `LICENSE` and `NOTICE`.
The machine-readable source paths, hashes, and modification classifications are
in `third_party/openai-codex/SOURCE.toml`; local adaptations are described in
`third_party/openai-codex/MODIFICATIONS.md`.

OpenAI Codex
Copyright 2025 OpenAI

The upstream Codex notice also identifies code derived from Ratatui, licensed
under the MIT License, with copyright held by Florian Dehau and the Ratatui
Developers.

## Rust dependencies

The binary links the Rust crates pinned in `Cargo.lock`, including the official
Model Context Protocol Rust SDK (`rmcp`), Tokio, Axum, gitoxide, cap-std,
portable-pty, serde, rustls, and their transitive dependencies. Their license
metadata and source distributions are available through the exact package
names and versions in `Cargo.lock`. The release process must review dependency
license metadata whenever `Cargo.lock` changes; this notice does not replace
any license text required by an individual dependency.
