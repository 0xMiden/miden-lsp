# Miden Assembly Language Server

This repo provides the implementation of `miden-lsp`, our Language Server Protocol implementation for Miden Assembly (MASM), including support for Rust-based Miden projects which compile to Miden Assembly.

This builds on top of [tree-sitter-masm](https://github.com/0xMiden/tree-sitter-masm) for parsing of Miden Assembly syntax files.

Miden LSP assumes that Miden projects are rooted by either a workspace `miden-project.toml`, or a single-package project `miden-project.toml`. Standalone MASM files will have limited functionality, as without project context, only local reasoning is possible.

## Editor Extensions

Editor integrations are being developed in-tree under `extensions/`.

- `extensions/miden-zed-extension`: Zed extension using `tree-sitter-masm` for editor syntax support and `miden-lsp` for language features

To load the Zed extension locally, open Zed's Extensions page, choose `Install Dev Extension` or run the `zed: install dev extension` action, and select `extensions/miden-zed-extension`.
The extension assumes `miden-lsp` is already available via `PATH`, or configured in Zed with `lsp.binary.path` for `miden-lsp`.

## Roadmap

- [ ] Go-to-definition
- [ ] Rename symbol
- [ ] Documentation on hover for imported modules and module items
- [ ] Stack effect overlay on hover, for each instruction in a procedure, to show
      the shape of the operand stack as determined by abstract interpretation
- [ ] Procedure type signatures, when available
