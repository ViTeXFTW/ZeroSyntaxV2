import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let baseIniRootsHintShown = false;
const allowBarePercentagesSetting = "analysis.allowPercentagesWithoutSign";

export function activate(context: vscode.ExtensionContext) {
  const serverPath = resolveServerPath(context);
  if (!serverPath) {
    vscode.window.showErrorMessage(
      "zerosyntax: could not find the language server binary. Set `zerosyntax.server.path` " +
        "or place `zerosyntax-lsp` under the extension's `server/` directory or on your PATH."
    );
    return;
  }

  const serverOptions: ServerOptions = {
    run: { command: serverPath, transport: TransportKind.stdio },
    debug: { command: serverPath, transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "generals-ini" },
      { scheme: "big", language: "generals-ini" },
    ],
    synchronize: {
      // Re-index when any .ini in the workspace changes on disk.
      fileEvents: vscode.workspace.createFileSystemWatcher("**/*.ini"),
      configurationSection: "zerosyntax",
    },
    // Keep startup compatibility for clients that do not synchronize settings.
    initializationOptions: () => ({
      format: {
        enable: setting<boolean>("format.enable", false),
      },
      preview: {
        enable: setting<boolean>("preview.enable", true),
        imageWidth: setting<number>("preview.imageWidth", 160),
        zoomPercent: setting<number>("preview.zoomPercent", 100),
      },
      progress: {
        mode: setting<string>("progress.mode", "indexing"),
      },
      baseIniRoots: setting<string[]>("baseIniRoots", []),
      schemaPath: setting<string>("schema.path", ""),
      analysis: {
        modelMemberStrictness: setting<string>("analysis.modelMemberStrictness", "compatible"),
        allowPercentagesWithoutSign: setting<boolean>(allowBarePercentagesSetting, false),
        mapOrderingDiagnostics: setting<boolean>("analysis.mapOrderingDiagnostics", true),
        debounceMs: setting<number>("analysis.debounceMs", 250),
      },
      clientBaseIniHint: true,
    }),
  };

  client = new LanguageClient(
    "zerosyntax",
    "ZeroSyntax v2 Language Server",
    serverOptions,
    clientOptions
  );

  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(
      "big",
      new BigIniDocumentProvider(() => client)
    )
  );

  client.start();
  context.subscriptions.push({
    dispose: () => {
      void client?.stop();
    },
  });

  context.subscriptions.push(
    vscode.commands.registerCommand("zerosyntax.selectGameDirectory", selectGameDirectory),
    vscode.commands.registerCommand("zerosyntax.openIndexCacheLocation", async () => {
      const cachePath = await client?.sendRequest<string>("zerosyntax/indexCachePath");
      if (!cachePath) {
        return;
      }
      const cacheUri = vscode.Uri.file(cachePath);
      if (fs.existsSync(cachePath)) {
        await vscode.commands.executeCommand("revealFileInOS", cacheUri);
      } else {
        vscode.window.showInformationMessage(`ZeroSyntax index cache will be created at ${cachePath}.`);
      }
    }),
    vscode.commands.registerCommand("zerosyntax.selectSchema", async () => {
      const selected = await vscode.window.showOpenDialog({
        canSelectMany: false,
        filters: { JSON: ["json"] },
        openLabel: "Use schema",
      });
      if (selected?.[0]) {
        await vscode.workspace
          .getConfiguration("zerosyntax")
          .update("schema.path", selected[0].fsPath, vscode.ConfigurationTarget.Workspace);
      }
    }),
    vscode.commands.registerCommand("zerosyntax.allowBarePercentages", async (uri?: vscode.Uri) => {
      const configuration = vscode.workspace.getConfiguration("zerosyntax", uri);
      const inspected = configuration.inspect<boolean>(allowBarePercentagesSetting);
      const target = inspected?.workspaceFolderValue !== undefined
        ? vscode.ConfigurationTarget.WorkspaceFolder
        : inspected?.workspaceValue !== undefined
          ? vscode.ConfigurationTarget.Workspace
          : vscode.ConfigurationTarget.Global;
      await configuration.update(allowBarePercentagesSetting, true, target);
    }),
    vscode.languages.registerCodeActionsProvider(
      { scheme: "file", language: "generals-ini" },
      {
        provideCodeActions(document, _range, actionContext) {
          const diagnostics = actionContext.diagnostics.filter(
            (diagnostic) => diagnostic.code === "bad-percent"
          );
          if (diagnostics.length === 0) {
            return [];
          }
          const action = new vscode.CodeAction(
            "Allow percentages without `%`",
            vscode.CodeActionKind.QuickFix
          );
          action.diagnostics = diagnostics;
          action.command = {
            command: "zerosyntax.allowBarePercentages",
            title: action.title,
            arguments: [document.uri],
          };
          return [action];
        },
      },
      { providedCodeActionKinds: [vscode.CodeActionKind.QuickFix] }
    ),
    vscode.workspace.onDidOpenTextDocument((document) => {
      void maybeShowBaseIniRootsHint(document);
    })
  );
  for (const editor of vscode.window.visibleTextEditors) {
    void maybeShowBaseIniRootsHint(editor.document);
  }

  // Only another executable requires another process; synchronized runtime
  // settings are applied by the existing client configuration notification.
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("zerosyntax.server.path")) {
        void client?.restart();
      }
    })
  );
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

/** Resolve the server binary: explicit setting > env override > bundled > PATH. */
function resolveServerPath(context: vscode.ExtensionContext): string | undefined {
  const configured = setting<string>("server.path", "");
  if (configured && configured.trim().length > 0) {
    return configured;
  }

  // Set by the repo's launch.json so the Extension Development Host picks up
  // the locally built debug binary without copying it into the extension.
  const fromEnv = process.env.ZEROSYNTAX_LSP_PATH;
  if (fromEnv && fs.existsSync(fromEnv)) {
    return fromEnv;
  }

  const exe = process.platform === "win32" ? "zerosyntax-lsp.exe" : "zerosyntax-lsp";
  const bundled = context.asAbsolutePath(path.join("server", exe));
  if (fs.existsSync(bundled)) {
    return bundled;
  }

  // Fall back to PATH lookup; the OS resolves the bare command name.
  return exe;
}

async function maybeShowBaseIniRootsHint(document: vscode.TextDocument) {
  if (
    baseIniRootsHintShown ||
    document.uri.scheme !== "file" ||
    document.languageId !== "generals-ini"
  ) {
    return;
  }

  const roots = setting<string[]>("baseIniRoots", []);
  if (roots.length > 0) {
    return;
  }

  baseIniRootsHintShown = true;
  const selectDirectory = "Select Game Folder";
  const openSettings = "Open Settings";
  const choice = await vscode.window.showInformationMessage(
    "ZeroSyntax v2: select your Zero Hour installation folder to enable game asset completions, model previews, and base game reference checks.",
    selectDirectory,
    openSettings
  );
  if (choice === selectDirectory) {
    await vscode.commands.executeCommand("zerosyntax.selectGameDirectory");
  } else if (choice === openSettings) {
    await vscode.commands.executeCommand(
      "workbench.action.openSettings",
      "zerosyntax.baseIniRoots"
    );
  }
}

async function selectGameDirectory() {
  try {
    const selected = await vscode.window.showOpenDialog({
      title: "Select your Zero Hour installation or mod folder",
      canSelectFiles: false,
      canSelectFolders: true,
      canSelectMany: false,
      openLabel: "Use Game Folder",
    });
    if (!selected?.[0]) {
      return;
    }

    // Match the configuration sent to the server. Read it after the dialog so
    // edits made while the picker is open are preserved.
    const configuration = vscode.workspace.getConfiguration("zerosyntax");
    const roots = configuration.get<string[]>("baseIniRoots", []);
    const directory = selected[0].fsPath;
    const normalize = (root: string) => {
      const normalized = path.normalize(root).replace(/[\\/]+$/, "");
      return process.platform === "win32" ? normalized.toLowerCase() : normalized;
    };
    if (roots.some((root) => normalize(root) === normalize(directory))) {
      void vscode.window.showInformationMessage("ZeroSyntax v2: this game folder is already configured.");
      return;
    }

    const inspected = configuration.inspect<string[]>("baseIniRoots");
    const target = inspected?.workspaceValue !== undefined
      ? vscode.ConfigurationTarget.Workspace
      : vscode.ConfigurationTarget.Global;
    await configuration.update("baseIniRoots", [...roots, directory], target);
    void vscode.window.showInformationMessage(
      "ZeroSyntax v2: game folder added. Game data will be indexed in the background."
    );
  } catch (error) {
    void vscode.window.showErrorMessage(
      `ZeroSyntax v2: could not configure the game folder. ${error instanceof Error ? error.message : String(error)}`
    );
  }
}

function setting<T>(key: string, fallback: T): T {
  const current = vscode.workspace.getConfiguration("zerosyntax");
  const inspected = current.inspect<T>(key);
  if (
    inspected?.globalValue !== undefined ||
    inspected?.workspaceValue !== undefined ||
    inspected?.workspaceFolderValue !== undefined ||
    inspected?.globalLanguageValue !== undefined ||
    inspected?.workspaceLanguageValue !== undefined ||
    inspected?.workspaceFolderLanguageValue !== undefined
  ) {
    return current.get<T>(key, fallback);
  }
  return vscode.workspace.getConfiguration("zerosyntax").get<T>(key, fallback);
}

class BigIniDocumentProvider implements vscode.TextDocumentContentProvider {
  constructor(private readonly getClient: () => LanguageClient | undefined) {}

  async provideTextDocumentContent(uri: vscode.Uri): Promise<string> {
    const activeClient = this.getClient();
    if (!activeClient) {
      return "";
    }
    return (
      (await activeClient.sendRequest<string | null>("zerosyntax/readVirtualFile", {
        uri: uri.toString(),
      })) ?? ""
    );
  }
}
