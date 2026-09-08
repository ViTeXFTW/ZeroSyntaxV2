import * as assert from "assert";
import * as fs from "fs";
import * as path from "path";
import * as vm from "vm";
import type * as vscode from "vscode";
import type { activate } from "../../extension";

const selectCommand = "zerosyntax.selectGameDirectory";
const gameFolder = path.resolve("Game Folder");
const modArchive = path.resolve("Mod.big");
const document = {
  languageId: "generals-ini",
  uri: { scheme: "file", fsPath: path.resolve("Object.ini") },
};

// Load a fresh extension for each test, with an isolated VS Code boundary.
// Exercise its registered commands/events without a real picker, server, or
// changes to the extension host's settings. The type import above also makes
// compile:test emit the production extension beside the test output.
function setup(options: {
  roots?: string[];
  workspaceRoots?: string[];
  platform?: "win32" | "linux";
  visible?: boolean;
} = {}) {
  const state = {
    roots: options.roots ?? [],
    selected: [{ fsPath: gameFolder }] as { fsPath: string }[] | undefined,
    choice: undefined as string | undefined,
    saveError: undefined as Error | undefined,
    beforeSelection: () => {},
  };
  const updates: { key: string; value: string[]; target: number }[] = [];
  const notifications: { message: string; actions: string[] }[] = [];
  const errors: string[] = [];
  const dialogs: vscode.OpenDialogOptions[] = [];
  const executed: { name: string; args: unknown[] }[] = [];
  const commands = new Map<string, () => Promise<void>>();
  let openDocument: (doc: typeof document) => void = () => assert.fail("open listener missing");
  const disposable = { dispose() {} };
  const api = {
    ConfigurationTarget: { Global: 1, Workspace: 2 },
    CodeActionKind: { QuickFix: {} },
    window: {
      visibleTextEditors: options.visible ? [{ document }] : [],
      async showOpenDialog(dialog: vscode.OpenDialogOptions) {
        dialogs.push(dialog);
        state.beforeSelection();
        return state.selected;
      },
      async showInformationMessage(message: string, ...actions: string[]) {
        notifications.push({ message, actions });
        return actions.length ? state.choice : undefined;
      },
      async showErrorMessage(message: string) { errors.push(message); },
    },
    workspace: {
      getConfiguration(section: string) {
        assert.strictEqual(section, "zerosyntax");
        return {
          get(key: string, fallback: unknown) {
            if (key === "server.path") { return "test-server"; }
            return key === "baseIniRoots" ? state.roots : fallback;
          },
          inspect(key: string) {
            return key === "baseIniRoots" ? { workspaceValue: options.workspaceRoots } : undefined;
          },
          async update(key: string, value: string[], target: number) {
            if (state.saveError) { throw state.saveError; }
            updates.push({ key, value, target });
            state.roots = value;
          },
        };
      },
      createFileSystemWatcher: () => disposable,
      registerTextDocumentContentProvider: () => disposable,
      onDidChangeConfiguration: () => disposable,
      onDidOpenTextDocument(handler: typeof openDocument) {
        openDocument = handler;
        return disposable;
      },
    },
    languages: { registerCodeActionsProvider: () => disposable },
    commands: {
      registerCommand(name: string, handler: () => Promise<void>) {
        commands.set(name, handler);
        return disposable;
      },
      async executeCommand(name: string, ...args: unknown[]) {
        executed.push({ name, args });
        await commands.get(name)?.();
      },
    },
  };
  const filename = path.resolve(__dirname, "../../extension.js");
  const extensionModule = { exports: {} as { activate: typeof activate } };
  const evaluate = vm.runInThisContext(
    `(function(require, module, exports, process) { ${fs.readFileSync(filename, "utf8")}\n})`,
    { filename }
  );
  evaluate((name: string) => {
    if (name === "vscode") { return api; }
    if (name === "vscode-languageclient/node") {
      return { LanguageClient: class { start() {} }, TransportKind: { stdio: 0 } };
    }
    if (name === "path" && options.platform) {
      return options.platform === "win32" ? path.win32 : path.posix;
    }
    return require(name);
  }, extensionModule, extensionModule.exports, { platform: options.platform ?? process.platform });
  extensionModule.exports.activate({ subscriptions: [] } as unknown as vscode.ExtensionContext);
  return {
    state, updates, notifications, errors, dialogs, executed,
    open: (doc = document) => openDocument(doc),
    async select() {
      const command = commands.get(selectCommand);
      assert.ok(command, "game-folder command must be registered");
      await command();
    },
  };
}

// Document-open handlers intentionally return void; flush their async work.
const settle = () => new Promise<void>((resolve) => setImmediate(resolve));

suite("Game folder setup", () => {
  test("uses a folder-only picker and appends to user settings", async () => {
    const app = setup({ roots: [modArchive] });
    await app.select();
    assert.strictEqual(app.dialogs[0].canSelectFiles, false);
    assert.strictEqual(app.dialogs[0].canSelectFolders, true);
    assert.strictEqual(app.dialogs[0].canSelectMany, false);
    assert.deepStrictEqual(app.updates, [
      { key: "baseIniRoots", value: [modArchive, gameFolder], target: 1 },
    ]);
    assert.match(app.notifications[0].message, /indexed in the background/);
  });

  for (const roots of [[], [modArchive]]) {
    test(`respects ${roots.length ? "populated" : "empty"} workspace overrides`, async () => {
      const app = setup({ roots, workspaceRoots: roots });
      await app.select();
      assert.deepStrictEqual(app.updates, [
        { key: "baseIniRoots", value: [...roots, gameFolder], target: 2 },
      ]);
    });
  }

  for (const selected of [undefined, []]) {
    test(`cancelling with ${selected === undefined ? "undefined" : "no selections"} changes nothing`, async () => {
      const app = setup({ roots: [modArchive] });
      app.state.selected = selected;
      await app.select();
      assert.deepStrictEqual(app.state.roots, [modArchive]);
      assert.deepStrictEqual(app.updates, []);
      assert.deepStrictEqual(app.notifications, []);
      assert.deepStrictEqual(app.errors, []);
    });
  }

  test("preserves settings changed while the picker is open", async () => {
    const app = setup();
    app.state.beforeSelection = () => { app.state.roots = [modArchive]; };
    await app.select();
    assert.deepStrictEqual(app.updates[0].value, [modArchive, gameFolder]);
  });

  test("does not duplicate Windows paths with different case or separators", async () => {
    const app = setup({ roots: ["c:/games/zero hour/"], platform: "win32" });
    app.state.selected = [{ fsPath: "C:\\Games\\Zero Hour" }];
    await app.select();
    assert.deepStrictEqual(app.updates, []);
    assert.match(app.notifications[0].message, /already configured/);
  });

  test("normalizes POSIX paths without folding their case", async () => {
    const app = setup({ roots: ["/games/ZeroHour/"], platform: "linux" });
    app.state.selected = [{ fsPath: "/games/ZeroHour" }];
    await app.select();
    assert.strictEqual(app.updates.length, 0);
    app.state.selected = [{ fsPath: "/games/zerohour" }];
    await app.select();
    assert.deepStrictEqual(app.updates[0].value, ["/games/ZeroHour/", "/games/zerohour"]);
  });

  test("reports save failures without claiming success", async () => {
    const app = setup();
    app.state.saveError = new Error("Settings are read-only");
    await app.select();
    assert.deepStrictEqual(app.updates, []);
    assert.deepStrictEqual(app.notifications, []);
    assert.strictEqual(app.errors.length, 1);
    assert.match(app.errors[0], /Settings are read-only/);
  });

  test("offers setup for ordinary INIs and routes the picker action", async () => {
    const app = setup();
    app.state.choice = "Select Game Folder";
    app.open();
    await settle();
    assert.deepStrictEqual(app.notifications[0].actions, ["Select Game Folder", "Open Settings"]);
    assert.deepStrictEqual(app.executed, [{ name: selectCommand, args: [] }]);
    assert.strictEqual(app.updates.length, 1);
  });

  test("routes the Settings action to base INI roots", async () => {
    const app = setup();
    app.state.choice = "Open Settings";
    app.open();
    await settle();
    assert.deepStrictEqual(app.executed, [
      { name: "workbench.action.openSettings", args: ["zerosyntax.baseIniRoots"] },
    ]);
    assert.deepStrictEqual(app.dialogs, []);
  });

  test("prompts once per activation even for concurrent opens and dismissal", async () => {
    const app = setup();
    app.open();
    app.open();
    await settle();
    app.open();
    await settle();
    assert.strictEqual(app.notifications.length, 1);
    assert.deepStrictEqual(app.executed, []);
    assert.deepStrictEqual(app.updates, []);
  });

  test("checks editors already visible at activation", async () => {
    const app = setup({ visible: true });
    await settle();
    assert.strictEqual(app.notifications.length, 1);
  });

  test("skips unrelated languages and virtual documents without consuming the hint", async () => {
    const app = setup();
    app.open({ ...document, languageId: "plaintext" });
    app.open({ ...document, uri: { ...document.uri, scheme: "big" } });
    await settle();
    assert.deepStrictEqual(app.notifications, []);
    app.open();
    await settle();
    assert.strictEqual(app.notifications.length, 1);
  });

  test("does not prompt when base roots are configured", async () => {
    const app = setup({ roots: [modArchive], visible: true });
    app.open();
    await settle();
    assert.deepStrictEqual(app.notifications, []);
    assert.deepStrictEqual(app.executed, []);
  });
});
