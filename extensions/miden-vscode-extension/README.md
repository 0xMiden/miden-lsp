# miden-vscode-extension

VS Code extension for Miden Assembly based on `miden-lsp`.

## Current Assumptions

- `miden-lsp` is already available via `PATH`, or configured with
  `miden-lsp.binary.path`
- rich syntax coloring comes from semantic tokens exposed by `miden-lsp`
- the bundled TextMate grammar is only a lightweight lexical fallback for the
  editor before the language server is active
- direct runtime integration of `tree-sitter-masm` is deferred; the recorded
  grammar lineage lives in `tree-sitter-masm.lock.json`

## Local Development

1. Run `npm install`.
2. Run `npm run compile`.
3. Open this folder in VS Code and start an Extension Development Host.

## Settings

- `miden-lsp.binary.path`: absolute or `PATH`-resolvable command for the server
- `miden-lsp.binary.args`: extra command-line arguments for the server
- `miden-lsp.binary.env`: extra environment variables for the server process
- `miden-lsp.initializationOptions`: JSON object forwarded during LSP
  initialization
- `miden-lsp.trace.server`: protocol trace level (`off`, `messages`, or
  `verbose`)
