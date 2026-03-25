# Miden Assembly Language Server

`miden-lsp` is a Language Server Protocol implementation for Miden Assembly
(MASM), with project-aware analysis for Miden workspaces rooted by
`miden-project.toml`.

The server uses `tree-sitter-masm` for live document parsing and
`miden-project` for manifest, target, namespace, and dependency resolution. When
dependency source is unavailable, it can fall back to `.masp` package metadata
via `miden-mast-package`.

## Implemented Features

Current server functionality includes:

- Project and workspace discovery from `miden-project.toml`
- Target-aware source-to-namespace mapping for `[lib]` and `[[bin]]` targets
- Dependency resolution across workspace, path, git, and registry dependencies
- In-memory registry support seeded from preassembled `.masp` artifacts
- Syntax diagnostics from `tree-sitter-masm`
- Project diagnostics for manifest loading and validation failures
- Stack-effect diagnostics for unbalanced control flow and indeterminate callees
- Document symbols and workspace symbols
- Go-to-definition across local source, workspace members, and dependencies
- Hover for local and dependency symbols, including metadata-backed signatures and
  attributes when available
- Find references within a workspace
- Prepare rename and rename for user-defined `proc`, `const`, and `type`
  symbols
- Completion for local, imported, and dependency-backed symbols
- Semantic tokens
- Inlay hints for resolved procedure signatures and stack-effect overlays
- Code lenses for exported procedures and executable entrypoints

Today, `miden-lsp` is strongest when operating inside a Miden project or
workspace. Standalone `.masm` files still have limited functionality, and
Rust-based Miden projects are currently project-aware rather than deeply
integrated with Rust editor tooling.

## Getting Started

### Prerequisites

- A Rust toolchain recent enough to build this crate
- A checkout of `miden-lsp` alongside the sibling `miden-vm` repository, since
  this crate currently depends on local `../miden-vm` paths
- A Miden project or workspace rooted by `miden-project.toml`

### Build

```sh
cargo build
```

To install the binary into your cargo bin directory instead:

```sh
cargo install --path .
```

### Run

`miden-lsp` speaks standard LSP over stdio and does not require extra command
line flags:

```sh
cargo run
```

Or run the built binary directly:

```sh
./target/debug/miden-lsp
```

### Initialization Options

The server currently supports the following initialization options:

```json
{
  "registryArtifacts": ["/abs/path/to/package-a.masp", "/abs/path/to/package-b.masp"],
  "gitCacheRoot": "/abs/path/to/git-cache"
}
```

- `registryArtifacts`: preassembled `.masp` artifacts to load into the in-memory
  registry used for registry-backed dependency resolution
- `gitCacheRoot`: optional directory for cached git dependencies

### Editor Integrations

Editor integrations are being developed as well:

- [`zed-extension`](https://github.com/0xMiden/zed-extension): Zed extension using `tree-sitter-masm` for
  editor syntax support and `miden-lsp` for semantic features
- [`vscode-extension`](https://github.com/0xMiden/vscode-extension): VS Code extension using a lightweight
  MASM language contribution plus `miden-lsp` for semantic features

See the installation and usage instructions in their respective repos.

## Future Roadmap

The core server is in place, but there are several areas worth exploring next:

- Better standalone `.masm` support with degraded local analysis outside a full
  Miden project
- Richer semantic diagnostics for unresolved imports, duplicate definitions,
  ambiguous references, and target/source ownership conflicts
- Deeper mixed-project support for Rust-based Miden projects, including a clean
  integration story alongside Rust LSP tooling
- Migration from the current in-memory registry seeding model to the filesystem
  registry implementation once that is ready
- More precise stack-effect analysis for additional provenance-preserving stack
  ops, repeat-count reasoning, and follow-on code actions
- Signature help, call hierarchy, and other procedure-centric navigation surfaces
- Import management and other code actions/refactorings built on the existing
  semantic index
- Stronger editor-extension validation and release packaging for Zed and VS Code

## Status

The server implementation is backed by automated Rust tests and in-repo editor
extension scaffolds for Zed and VS Code. The main remaining validation gap is
manual smoke testing against real MASM-only and mixed Rust/Miden projects in the
target editors.
