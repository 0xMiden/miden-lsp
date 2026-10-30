import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

const { values } = parseArgs({
  options: {
    code: { type: 'string' },
    midenup: { type: 'string' },
    lsp: { type: 'string' },
    extension: { type: 'string' },
    'named-toolchain': { type: 'boolean', default: false },
    'git-dependency': { type: 'boolean', default: false },
  },
});
for (const name of ['code', 'midenup', 'lsp', 'extension']) {
  assert.ok(values[name], `--${name} is required`);
  values[name] = fs.realpathSync(values[name]);
}
assert.ok(fs.existsSync(path.join(values.extension, 'out', 'extension.js')), 'compile the extension first');
const temp = process.platform === 'darwin' ? '/tmp' : os.tmpdir();
const root = fs.realpathSync(fs.mkdtempSync(path.join(temp, 'miden-lsp-vscode-')));
const project = path.join(root, 'project');
const cargoHome = path.join(root, 'cargo');
const bin = path.join(cargoHome, 'bin');
const home = path.join(root, 'midenup');
const manifest = path.join(root, 'manifest.json');
fs.mkdirSync(path.join(project, '.vscode'), { recursive: true });
fs.mkdirSync(bin, { recursive: true });
fs.symlinkSync(values.midenup, path.join(bin, 'miden'));
fs.symlinkSync(values.midenup, path.join(bin, 'midenup'));
fs.writeFileSync(manifest, JSON.stringify({
  manifest_version: '3.0.0',
  min_client_version: '1.3.0',
  networks: { devnet: '0.18.0', mainnet: '0.18.0', testnet: '0.18.0' },
  date: 1793318400,
  channels: [{ name: '0.18.0', components: [{
    name: 'miden-lsp',
    version: { kind: 'registry', version: '0.1.2' },
    kind: 'executable',
    installation_method: { kind: 'prebuilt' },
    'installed-executable': 'miden-lsp',
    profiles: ['complete'],
    artifacts: { 'miden-lsp': { uri: pathToFileURL(values.lsp).href } },
  }] }],
}));
fs.writeFileSync(path.join(project, 'miden-toolchain.toml'),
  '[toolchain]\n' + (values['named-toolchain'] ? 'name = "editor-dev"\n' : '') +
  'channel = "0.18.0"\ncomponents = ["miden-lsp"]\nprofile = "minimal"\n');
fs.writeFileSync(path.join(project, 'miden-project.toml'),
  '[package]\nname = "app"\nversion = "1.0.0"\n[lib]\npath = "mod.masm"\nnamespace = "app"\n');
fs.writeFileSync(path.join(project, 'mod.masm'),
  'pub mod child\npub proc entry() -> u32\nadv_push\npush.1\nu32wrapping_add\nend\npub proc main\nexec.child::target\nend\n');
fs.writeFileSync(path.join(project, 'child.masm'), 'pub proc target()\npush.1 drop\nend\n');
if (values['git-dependency']) {
  const dependency = path.join(root, 'dependency');
  fs.mkdirSync(dependency);
  fs.writeFileSync(path.join(dependency, 'miden-project.toml'),
    '[package]\nname = "dep"\nversion = "1.0.0"\n[lib]\npath = "mod.masm"\nnamespace = "dep"\n');
  fs.writeFileSync(path.join(dependency, 'mod.masm'), 'pub proc target()\npush.1 drop\nend\n');
  for (const args of [['init', '-b', 'main'], ['config', 'user.name', 'LSP editor test'],
    ['config', 'user.email', 'test@example.invalid'], ['config', 'commit.gpgsign', 'false'],
    ['add', '.'], ['commit', '-m', 'fixture']]) {
    const result = spawnSync('git', args, { cwd: dependency, encoding: 'utf8' });
    assert.ifError(result.error);
    assert.equal(result.status, 0, result.stderr);
  }
  fs.appendFileSync(path.join(project, 'miden-project.toml'),
    `[dependencies]\ndep = { git = '${pathToFileURL(dependency).href}', branch = 'main' }\n`);
  fs.writeFileSync(path.join(project, 'mod.masm'),
    'use dep\npub proc entry() -> u32\nadv_push\npush.1\nu32wrapping_add\nend\npub proc main\nexec.dep::target\nend\n');
}
fs.writeFileSync(path.join(project, '.vscode', 'settings.json'), JSON.stringify({
  'miden-lsp.binary.path': values.lsp,
  'miden-lsp.binary.args': ['--miden', path.join(bin, 'miden')],
  'miden-lsp.binary.env': {
    MIDENUP_HOME: home,
    CARGO_HOME: cargoHome,
    MIDENUP_MANIFEST_URI: pathToFileURL(manifest).href,
    PATH: bin + path.delimiter + process.env.PATH,
    RUST_MIN_STACK: '16777216',
  },
  'miden-lsp.trace.server': 'verbose',
  'miden-lsp.initializationOptions': { gitCacheRoot: path.join(root, 'git-cache') },
}));
const log = fs.openSync(path.join(root, 'vscode.log'), 'w');
console.log(`Editor test artifacts: ${root}`);
const editorEnv = { ...process.env, MIDEN_LSP_EDITOR_TEST_ROOT: root };
for (const name of ['MIDENUP_TOOLCHAIN', 'MIDEN_SYSROOT', 'MIDEN_LSP_TOOLCHAIN_OVERRIDE']) {
  delete editorEnv[name];
}
const result = spawnSync(values.code, [
  `--extensionDevelopmentPath=${values.extension}`,
  `--extensionTestsPath=${fileURLToPath(new URL('../tests/editor.cjs', import.meta.url))}`,
  `--user-data-dir=${path.join(root, 'profile')}`,
  `--extensions-dir=${path.join(root, 'extensions')}`,
  '--disable-extensions', '--disable-workspace-trust', '--disable-updates',
  '--skip-welcome', '--skip-release-notes', project,
], {
  env: editorEnv,
  stdio: ['ignore', log, log],
  timeout: 180000,
});
fs.closeSync(log);
assert.ifError(result.error);
const report = JSON.parse(fs.readFileSync(path.join(root, 'result.json'), 'utf8'));
console.log(JSON.stringify(report, null, 2));
assert.equal(report.status, 'passed', report.error);
assert.equal(result.status, 0);
