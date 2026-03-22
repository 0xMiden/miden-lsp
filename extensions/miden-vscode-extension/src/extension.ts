import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  Trace,
} from "vscode-languageclient/node";

const CONFIG_SECTION = "miden-lsp";
const SERVER_COMMAND = "miden-lsp";
const SERVER_ID = "miden-lsp";
const SERVER_NAME = "Miden Language Server";

let client: LanguageClient | undefined;
let outputChannel: vscode.OutputChannel | undefined;
let restartInFlight: Promise<void> | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  outputChannel = vscode.window.createOutputChannel("Miden LSP");
  context.subscriptions.push(outputChannel);

  context.subscriptions.push(
    vscode.commands.registerCommand("miden-lsp.restartServer", async () => {
      await restartClient(true);
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (!event.affectsConfiguration(CONFIG_SECTION)) {
        return;
      }

      await restartClient(false);
    }),
  );

  context.subscriptions.push(
    new vscode.Disposable(() => {
      void stopClient();
    }),
  );

  await startClient();
}

export async function deactivate(): Promise<void> {
  await stopClient();
}

async function restartClient(notifyUser: boolean): Promise<void> {
  if (restartInFlight) {
    await restartInFlight;
    return;
  }

  restartInFlight = (async () => {
    await stopClient();
    await startClient();

    if (notifyUser) {
      void vscode.window.showInformationMessage("Miden language server restarted.");
    }
  })();

  try {
    await restartInFlight;
  } finally {
    restartInFlight = undefined;
  }
}

async function startClient(): Promise<void> {
  if (client) {
    return;
  }

  const serverOptions = getServerOptions();
  const clientOptions = getClientOptions();
  const nextClient = new LanguageClient(
    SERVER_ID,
    SERVER_NAME,
    serverOptions,
    clientOptions,
  );
  nextClient.setTrace(getTraceLevel());

  try {
    await nextClient.start();
    client = nextClient;
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error);
    outputChannel?.appendLine(`Failed to start ${SERVER_COMMAND}: ${detail}`);
    void vscode.window.showErrorMessage(
      `Failed to start ${SERVER_COMMAND}. See the Miden LSP output channel for details.`,
    );
    throw error;
  }
}

async function stopClient(): Promise<void> {
  if (!client) {
    return;
  }

  const current = client;
  client = undefined;
  await current.stop();
}

function getServerOptions(): ServerOptions {
  const configuration = vscode.workspace.getConfiguration(CONFIG_SECTION);
  const configuredPath = normalizeString(configuration.get<string>("binary.path"));
  const configuredArgs = normalizeStringArray(configuration.get<unknown[]>("binary.args"));
  const configuredEnv = normalizeEnvironment(
    configuration.get<Record<string, unknown>>("binary.env"),
  );

  return {
    command: configuredPath ?? SERVER_COMMAND,
    args: configuredArgs,
    options: {
      env: {
        ...process.env,
        ...configuredEnv,
      },
    },
  };
}

function getClientOptions(): LanguageClientOptions {
  return {
    documentSelector: [
      { scheme: "file", language: "masm" },
      { scheme: "untitled", language: "masm" },
    ],
    outputChannel,
    traceOutputChannel: outputChannel,
    synchronize: {
      configurationSection: CONFIG_SECTION,
    },
    initializationOptions: getInitializationOptions(),
  };
}

function getInitializationOptions(): Record<string, unknown> {
  const configuration = vscode.workspace.getConfiguration(CONFIG_SECTION);
  const initializationOptions = configuration.get<unknown>("initializationOptions");
  if (
    initializationOptions !== null &&
    typeof initializationOptions === "object" &&
    !Array.isArray(initializationOptions)
  ) {
    return initializationOptions as Record<string, unknown>;
  }

  return {};
}

function getTraceLevel(): Trace {
  const configuration = vscode.workspace.getConfiguration(CONFIG_SECTION);
  switch (configuration.get<string>("trace.server")) {
    case "messages":
      return Trace.Messages;
    case "verbose":
      return Trace.Verbose;
    default:
      return Trace.Off;
  }
}

function normalizeString(value: string | undefined): string | undefined {
  if (!value) {
    return undefined;
  }

  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : undefined;
}

function normalizeStringArray(values: unknown[] | undefined): string[] {
  if (!Array.isArray(values)) {
    return [];
  }

  return values.filter((value): value is string => typeof value === "string");
}

function normalizeEnvironment(
  values: Record<string, unknown> | undefined,
): Record<string, string> {
  if (!values || typeof values !== "object") {
    return {};
  }

  return Object.fromEntries(
    Object.entries(values).filter(
      (entry): entry is [string, string] => typeof entry[1] === "string",
    ),
  );
}
