# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Run the compiler's MASM-to-HIR disassembler and interprocedural advice-taint
  analysis for live project diagnostics, with unsaved dependency overlays.
- Select server toolchains through midenup and isolate cached package registries
  by project toolchain. Refresh open documents after dependency and configuration changes.
- Preserve successful-analysis warnings, diagnostic severities, codes, help, and
  UTF-16 source ranges through a structured diagnostics adapter.
- Pin compatible compiler libraries and their required nightly Rust toolchain.
- Rebuild parsing, symbol indexing, and module discovery on the VM 0.36.3 assembler's lossless CST and module APIs, removing Tree-sitter dependencies.
- Validate MASM projects through the project assembler using unsaved editor buffers, and resolve compiled dependencies through unified package debug information.
- Load toolchain packages from a configured sysroot or `MIDEN_SYSROOT`, with guidance for incompatible package formats.

### Fixed

- Honor named midenup installations and externally selected toolchain sysroots.
- Share Git dependency checkouts and unsaved buffers across navigation and assembly;
  reuse cached dependencies offline without fetching or resetting local edits.
- Preserve midenup's project-specific component, profile, and patch settings when
  dispatching the language server.
- Exit promptly after the LSP exit notification without waiting for stdin EOF.
- Resolve declared submodule names in navigation and stack-effect analysis.
- Keep document text and syntax trees synchronized when an incremental edit is invalid, and reject positions splitting UTF-16 surrogate pairs.
- Distinguish source locations of procedures that share the same executable MAST digest.

## [0.1.2](https://github.com/0xMiden/miden-lsp/compare/v0.1.1...v0.1.2) - 2026-04-23

### Other

- pre-release 0.1.2

## [0.1.1](https://github.com/0xMiden/miden-lsp/compare/v0.1.0...v0.1.1) - 2026-04-22

### Added

- improve stack-effect inlay
- implement experimental vscode extension
- implement experimental zed extension
- implement stack-effect analysis with hover/inlay hints
- implement semantic tokens, signature-driven inlay hints, code lenses, simple command handling
- implement find references, rename, and completion
- implement basic lsp functionality

### Fixed

- formatting of on-hover documentation

### Other

- switch to crates.io-based miden packages
- migrate miden-zed-extension to its own repo
- migrate miden-vscode-extension to its own repo
- ensure rust-toolchain.toml is provided for zed
- ignore binary artifacts
- update readme to reflect latest changes
- expand test coverage, including protocol-level tests
