# ZeroSyntax v2 for VS Code

ZeroSyntax adds diagnostics, completion, navigation, refactoring, semantic
highlighting, and quick fixes for *Command & Conquer: Generals – Zero Hour* INI
files. The platform-specific extension package includes the language server, so
there is nothing else to install.

## Install

1. Download the `.vsix` for Windows x64 or Linux x64 from the
   [latest release](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest).
2. In VS Code, open **Extensions**, choose **Views and More Actions …**, then
   select **Install from VSIX…**.
3. Open the downloaded file and reload VS Code if prompted.
4. Open your map or mod folder and start editing an `.ini` file.

You can also install from a terminal:

```sh
code --install-extension zerosyntax-vscode-<platform>-<version>.vsix
```

## Recommended setup

Open the folder containing your project rather than a single file. ZeroSyntax
indexes the workspace so definitions, references, rename, and completion work
across its INI files.

When you first open an INI file, click **Select Game Folder** in the setup
notification and browse to your Zero Hour installation folder. You can also run
**ZeroSyntax: Select Game Folder** from the Command Palette at any time to add
a game or mod folder. The folder is added to `zerosyntax.baseIniRoots` without
replacing existing entries, and indexing starts automatically without a restart.
It is saved in user settings for reuse across projects, unless the workspace
already overrides this setting; in that case, the workspace setting is updated.

Use **ZeroSyntax v2: Base Ini Roots** in Settings to manage additional roots,
reorder or remove entries, or add individual `.big` archives that load before
`map.ini` or `solo.ini`. This
prevents false unresolved-reference warnings and enables W3D model, bone,
WAV/MP3 audio, and TGA/DDS texture checks. Configure every loaded game/mod root;
asset warnings activate per kind once any matching asset is indexed. DDS-only
textures are offered using the engine-compatible `stem.tga` spelling.

While completing `Model =`, move the active selection with the keyboard or
mouse to see a textured W3D thumbnail in VS Code's suggestion-details pane.
The preview is loaded only for the selected entry. If the pane is collapsed,
press `Ctrl+Space` again or use the suggestion widget's details control.
Disable `zerosyntax.preview.enable` to avoid model rendering and cached images
on lower-end hardware. Use `zerosyntax.preview.imageWidth` to change the thumbnail/pane content width
and `zerosyntax.preview.zoomPercent` to change the model framing.

## Settings

| Setting | Default | Purpose |
| --- | --- | --- |
| `zerosyntax.baseIniRoots` | `[]` | Base game/mod directories and `.big` archives used for INI and game-asset checks; changes reindex. |
| `zerosyntax.progress.mode` | `indexing` | Shows indexing progress by default; `verbose` also reports settings-driven diagnostic refreshes, and `off` hides status progress while keeping Output logs. |
| `zerosyntax.schema.path` | empty | Custom schema JSON; changes reparse and reindex, with invalid files falling back to the built-in schema. |
| `zerosyntax.analysis.modelMemberStrictness` | `compatible` | Disables member warnings, accepts any applicable model, or requires every model; applies immediately. |
| `zerosyntax.analysis.allowPercentagesWithoutSign` | `false` | Allows engine-compatible percentage values without a trailing `%`; applies immediately. |
| `zerosyntax.analysis.mapOrderingDiagnostics` | `true` | Warns about source-proven forward-order problems in `map.ini` and `solo.ini`; applies immediately. |
| `zerosyntax.analysis.debounceMs` | `250` | Delay before diagnostics/index refresh after typing; applies to future edits immediately. |
| `zerosyntax.preview.enable` | `true` | Enables rendered W3D model completion previews; disable on lower-end hardware. |
| `zerosyntax.preview.imageWidth` | `160` | Displayed W3D thumbnail width in pixels; applies to newly resolved previews immediately. |
| `zerosyntax.preview.zoomPercent` | `100` | Default W3D preview camera zoom; applies to newly resolved previews immediately. |
| `zerosyntax.format.enable` | `false` | Enables indentation formatting immediately when the client supports dynamic registration. |
| `zerosyntax.server.path` | empty | Uses a custom `zerosyntax-lsp` binary instead of the bundled one; changing it restarts the server. |
| `zerosyntax.trace.server` | `off` | Logs LSP traffic for troubleshooting. |

Formatting is intentionally off by default. Enable it only when you want
**Format Document** or format-on-save to normalize indentation.

Runtime settings reload without restarting. Schema and base-root changes show
phased progress through discovery, indexing, activation, and diagnostics unless
`zerosyntax.progress.mode` is `off`; only changing the server executable path
requires a normal VS Code language-server restart.

## INI file association

The extension associates `.ini` files with **Generals INI**. If your workspace
also contains unrelated INI files, keep those as plain text and override the
association only for the folders you want ZeroSyntax to handle:

```json
{
  "files.associations": {
    "**/*.ini": "plaintext",
    "Data/INI/**/*.ini": "generals-ini",
    "Maps/**/*.ini": "generals-ini"
  }
}
```

Use the language selector in VS Code's status bar to change an individual file.

## Troubleshooting

- If the server cannot be found, reinstall the `.vsix` for your platform. A
  source checkout does not contain a bundled server until it is built.
- If map references are reported as missing, configure
  `zerosyntax.baseIniRoots` and check that the paths point to the required INI
  folders or `.big` archives.
- Start with the operational log under **Output → ZeroSyntax v2 Language
  Server**. Set `zerosyntax.trace.server` to `verbose` for protocol traffic.
  For internal cache, parse, and diagnostic decisions, launch VS Code with
  `RUST_LOG=zerosyntax_lsp=debug` (PowerShell:
  `$env:RUST_LOG = "zerosyntax_lsp=debug"; code .`).

Report reproducible problems through
[GitHub Issues](https://github.com/ViTeXFTW/ZeroSyntaxV2/issues) with a minimal
INI sample, your OS, VS Code version, and ZeroSyntax version.

Contributor build and packaging instructions are in the
[VS Code development guide](https://github.com/ViTeXFTW/ZeroSyntaxV2/blob/dev/docs/vscode-development.md).
