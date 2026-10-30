# Miden Assembly Language Server

`miden-lsp` is a Language Server Protocol implementation for Miden Assembly
(MASM), with project-aware analysis for Miden workspaces rooted by
`miden-project.toml`.

The server uses the assembler's lossless CST for live parsing and its project
assembler for semantic diagnostics, including unsaved document overlays.
`miden-project` provides manifest and dependency resolution; compiled dependencies
are indexed through the package manifest and unified package debug information.

## Implemented Features

Current server functionality includes:

- Project and workspace discovery from `miden-project.toml`
- Target-aware source-to-namespace mapping for `[lib]` and `[[bin]]` targets
- Dependency resolution across workspace, path, git, and registry dependencies
- In-memory registry support seeded from preassembled `.masp` artifacts
- Syntax diagnostics from the assembler's lossless CST parser
- Project-assembler diagnostics for unresolved symbols and invalid modules
- Project diagnostics for manifest loading and validation failures
- Stack-effect diagnostics for unbalanced control flow and indeterminate callees
- Compiler-backed, interprocedural HIR analysis for unconstrained advice values
- Semantic warnings even when project assembly succeeds
- Toolchain-aware startup and independent package registries for workspace roots
- Refresh diagnostics after source, manifest, toolchain, or package changes
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

- The pinned Rust toolchain (installed automatically by rustup)
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

The server supports the following initialization options:

```json
{
  "registryArtifacts": ["/abs/path/to/package-a.masp", "/abs/path/to/package-b.masp"],
  "gitCacheRoot": "/abs/path/to/git-cache",
  "midenupHome": "/abs/path/to/midenup",
  "sysroot": "/abs/path/to/miden-toolchain"
}
```

- `registryArtifacts`: preassembled `.masp` artifacts to load into the in-memory
  registry used for registry-backed dependency resolution
- `gitCacheRoot`: optional directory for cached git dependencies
- `midenupHome`: optional midenup installation directory; otherwise `MIDENUP_HOME`
  or `midenup show home` is used when a project declares a toolchain
- `sysroot`: optional toolchain directory; packages in its `lib/` directory are
  available to dependency resolution. Defaults to `MIDEN_SYSROOT` when launched
  through midenup when no project-specific toolchain is declared. An explicit
  initialization option takes precedence over project and environment settings.

### Toolchains

Start from a project directory, or pass it explicitly:

```sh
miden-lsp --project /path/to/project
miden-lsp --project /path/to/project --toolchain 0.18.0
```

The nearest `miden-toolchain.toml` selects the server through
`miden +CHANNEL miden-lsp`, preserving the stdio connection. The toolchain must
provide the `miden-lsp` component; midenup handles component selection and
installation according to its normal policy. `--miden` selects a non-default
midenup wrapper executable. For development with an unpublished server build:

```sh
cargo run -- --no-toolchain-dispatch --project /path/to/project
```

Within a multi-root workspace, each project's declared toolchain supplies its own
packages. Registries are cached separately and refreshed when artifacts change.
Incompatible package formats produce a matching-toolchain diagnostic; selecting
a sysroot does not translate package formats. Use separate server instances for
projects requiring incompatible VM generations.

### Compiler analysis and diagnostics

The server uses the compiler's MASM disassembler and HIR dataflow solver directly,
with project dependency metadata and unsaved source buffers. Unconstrained advice
findings are reported at their source spans. Unsupported procedures and exhausted
analysis budgets produce explicit warnings, not a clean-analysis claim. Analysis
does not build Rust projects or write compiled artifacts into the project tree.

Compiler libraries are pinned to a compatible upstream revision until their next
release; the Rust toolchain is pinned accordingly. The existing conservative stack
visualization remains alongside the compiler's dataflow diagnostics.

VM 0.36 does not expose a successful-parse diagnostic sink yet. The adapter collects
semantic warnings through warning-as-error parsing, preserves their warning
severity, and retains the separately successful assembly result. It also preserves
diagnostic codes, help, source URIs, and UTF-16 editor ranges. This does not depend
on the still-unreleased upstream diagnostics rewrite.

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

Tests include real server processes over stdio: compiler warnings and unsaved
fixes, dependency edits, file watcher registration, multi-root toolchain selection,
midenup dispatch, and shutdown without closing stdin. Run them with `cargo test
--locked --all-targets`. Manual VS Code and Zed smoke testing remains release QA.
