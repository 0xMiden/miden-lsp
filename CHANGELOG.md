# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
