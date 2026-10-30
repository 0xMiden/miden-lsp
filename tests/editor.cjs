const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vscode = require('vscode');

async function waitFor(description, predicate) {
  const deadline = Date.now() + 30000;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(`Timed out: ${description}`);
}

exports.run = async function () {
  const root = process.env.MIDEN_LSP_EDITOR_TEST_ROOT;
  assert.ok(root, 'run through tools/test-vscode.mjs');
  const results = [];
  let extension;
  try {
    extension = vscode.extensions.getExtension('miden.miden-vscode-extension');
    assert.ok(extension, 'the real development extension must be loaded');
    await extension.activate();
    const source = vscode.Uri.file(path.join(root, 'project', 'mod.masm'));
    const child = vscode.Uri.file(path.join(root, 'project', 'child.masm'));
    const document = await vscode.workspace.openTextDocument(source);
    const editor = await vscode.window.showTextDocument(document);
    await waitFor('compiler warning', () => vscode.languages.getDiagnostics(source).some(
      item => item.source === 'miden-compiler' && item.message.includes('unconstrained')));
    const state = JSON.parse(fs.readFileSync(path.join(root, 'midenup', 'state.json'), 'utf8'));
    assert.ok(JSON.stringify(state).includes('miden-lsp'));
    results.push('real midenup cold-installs and launches the actual LSP in VS Code');

    const definitions = await vscode.commands.executeCommand('vscode.executeDefinitionProvider', source, new vscode.Position(7, 14));
    assert.ok(definitions.some(item => (item.uri || item.targetUri).fsPath === child.fsPath));
    const hovers = await vscode.commands.executeCommand('vscode.executeHoverProvider', source, new vscode.Position(7, 14));
    assert.ok(hovers.some(item => item.contents.length > 0));
    const symbols = await vscode.commands.executeCommand('vscode.executeDocumentSymbolProvider', source);
    assert.ok(symbols.some(item => item.name === 'entry'));
    results.push('cross-module definition, hover, and document symbols');

    assert.ok(await editor.edit(builder => builder.insert(new vscode.Position(3, 0), 'u32assert\n')));
    await waitFor('unsaved fix', () => vscode.languages.getDiagnostics(source).length === 0);
    assert.ok(document.isDirty);
    assert.ok(!fs.readFileSync(source.fsPath, 'utf8').includes('u32assert'));
    results.push('unsaved edits clear compiler warnings without changing the source file');

    const original = fs.readFileSync(child.fsPath, 'utf8');
    fs.writeFileSync(child.fsPath, original.replace('target', 'other'));
    await waitFor('disk dependency change', () => vscode.languages.getDiagnostics(source).some(item => item.message.includes('target')));
    fs.writeFileSync(child.fsPath, original);
    await waitFor('restored dependency', () => vscode.languages.getDiagnostics(source).length === 0);
    results.push('real VS Code file watchers revalidate open callers');

    const childDocument = await vscode.workspace.openTextDocument(child);
    const childEditor = await vscode.window.showTextDocument(childDocument);
    assert.ok(await childEditor.edit(builder => builder.replace(new vscode.Range(0, 9, 0, 15), 'other')));
    await waitFor('unsaved dependency change', () => vscode.languages.getDiagnostics(source).some(item => item.message.includes('target')));
    assert.equal(fs.readFileSync(child.fsPath, 'utf8'), original);
    assert.ok(await childEditor.edit(builder => builder.replace(new vscode.Range(0, 9, 0, 14), 'target')));
    await waitFor('unsaved dependency restored', () => vscode.languages.getDiagnostics(source).length === 0);
    results.push('unsaved dependency overlays refresh the caller');

    await vscode.commands.executeCommand('miden-lsp.restartServer');
    const restarted = await vscode.commands.executeCommand('vscode.executeDefinitionProvider', source, new vscode.Position(8, 14));
    assert.ok(restarted.some(item => (item.uri || item.targetUri).fsPath === child.fsPath));
    await waitFor('buffer recovery after restart', () => vscode.languages.getDiagnostics(source).length === 0);
    results.push('extension restart restores unsaved buffers and semantic requests');
    assert.ok(!fs.existsSync(path.join(root, 'project', 'target')));
    await require(path.join(extension.extensionPath, 'out', 'extension.js')).deactivate();
    results.push('clean LSP shutdown without compiling project artifacts');
    fs.writeFileSync(path.join(root, 'result.json'), JSON.stringify({ status: 'passed', results }, null, 2));
  } catch (error) {
    fs.writeFileSync(path.join(root, 'result.json'), JSON.stringify({ status: 'failed', results, error: String(error), stack: error.stack }, null, 2));
    throw error;
  }
};
