# Standalone language server

Use `zerosyntax-lsp` with any editor that supports the Language Server Protocol
over stdio.

## Install

1. Download the Windows x64 or Linux x64 server archive from the
   [latest release](https://github.com/ViTeXFTW/ZeroSyntaxV2/releases/latest).
2. Verify the archive against `SHA256SUMS.txt` from the same release.
3. Extract the archive and put `zerosyntax-lsp` (or `zerosyntax-lsp.exe`) on
   your `PATH`, or configure its absolute path in your editor.
4. Register the server for the Generals/Zero Hour `.ini` files in your project.

The server writes protocol messages to stdout, so clients must launch it using
stdio rather than a TCP port.

## Logging and troubleshooting

The server sends concise lifecycle, configuration, indexing, and command
outcomes through LSP `window/logMessage`. In VS Code these appear under
**Output → ZeroSyntax v2 Language Server** at their proper Info, Warning, or
Error level. Long scans also keep their existing status-bar progress report.

Developer detail uses structured `tracing` records on stderr and is controlled
with the standard `RUST_LOG` filter. The default is `warn`; enable server Debug
or Trace records before starting the editor:

```powershell
$env:RUST_LOG = "zerosyntax_lsp=debug" # or zerosyntax_lsp=trace
code .
```

```sh
RUST_LOG=zerosyntax_lsp=debug code . # or zerosyntax_lsp=trace
```

The same filter works when launching `zerosyntax-lsp --stdio` from another LSP
client. Debug records include paths, document URIs, versions, timings, cache
decisions, parse strategies, and diagnostic counts. Info logs contain counts
instead of paths. Source text, INI values, completion contents, and document
excerpts are never logged.

`zerosyntax.trace.server = verbose` is separate: it records the LSP requests
and responses themselves, while `RUST_LOG` explains internal server decisions.
Raw stderr can be decorated as an error by VS Code's language-client transport;
the level printed inside each tracing record is authoritative. Stdout is always
reserved for LSP framing and must never receive logs.

## Command-line diagnostics

Use the `check` subcommand to run the same parser, schema, workspace index, and
diagnostics without an LSP client:

```sh
zerosyntax-lsp check Data/INI
zerosyntax-lsp check map.ini --base-root "C:/Games/Zero Hour"
zerosyntax-lsp check --fail-on warning Data/INI
zerosyntax-lsp check --json --stdin-filename map.ini - < generated.ini
```

The positional targets are `.ini` files, recursively scanned directories, or
one `-` for stdin. At least one target is required. All selected targets are
indexed together before diagnostics run, so references between them resolve.
Overlapping targets are checked once.

`--base-root` is repeatable and accepts directories or `.big` archives
containing base/mod INIs and game assets. Base roots participate in reference,
model, bone, audio, and texture checks but do not emit diagnostics themselves. For stdin,
`--stdin-filename` supplies the displayed/indexed name and enables `map.ini` or
`solo.ini` override semantics; it defaults to `<stdin>`.

Human output uses compiler-style records followed by a summary:

```text
Data/INI/Weapon.ini:12:14: error[bad-number]: expected an integer, found `lots`
1 error(s), 0 warning(s), 0 hint(s)
```

`--json` writes a stable array to stdout. Each record has `file`, `range`,
`severity`, `code`, and `message`. Range lines and Unicode-scalar columns are
1-based; the end position is exclusive.

```json
[{"file":"map.ini","range":{"start":{"line":2,"column":14},"end":{"line":2,"column":18}},"severity":"error","code":"bad-number","message":"expected an integer, found `lots`"}]
```

Exit codes are:

- `0`: no diagnostic meets the failure threshold.
- `1`: at least one diagnostic meets it.
- `2`: invalid arguments, inaccessible/unsupported inputs, or an internal
  failure.

The default threshold is `error`. Select `--fail-on warning` or
`--fail-on hint` for stricter CI. Diagnostics are still written to stdout when
the command exits 1; operational errors go to stderr.

## Client configuration

Configure your LSP client with:

| Option | Value |
| --- | --- |
| Command | `zerosyntax-lsp` |
| Transport | stdio |
| Files | Zero Hour `.ini` files |
| Workspace root | Your map or mod directory |

The server indexes INI files under each workspace folder. Open the whole project
to enable cross-file completion, definitions, references, rename, and workspace
symbols.

## Initialization options

```json
{
  "format": { "enable": false },
  "preview": { "imageWidth": 160, "zoomPercent": 100 },
  "progress": { "mode": "indexing" },
  "schemaPath": "C:/Mods/MyMod/schema.json",
  "analysis": {
    "modelMemberStrictness": "compatible",
    "allowPercentagesWithoutSign": false,
    "mapOrderingDiagnostics": true,
    "debounceMs": 250
  },
  "baseIniRoots": [
    "C:/Games/Zero Hour",
    "C:/Mods/MyMod/Data/INI",
    "C:/Mods/MyMod.big"
  ]
}
```

- `format.enable` controls document formatting. It defaults to `false` and is
  dynamically registered when the client supports it.
- `preview.imageWidth` controls the displayed W3D thumbnail width in pixels
  (`80`–`640`, default `160`). The client still owns the outer details-pane
  bounds.
- `preview.zoomPercent` controls the default W3D camera zoom (`25`–`400`,
  default `100`). Both preview settings apply to newly resolved completions
  without restarting the server.
- `progress.mode` is `off`, `indexing` (the default), or `verbose`. The default
  reports phased startup, schema/base-root reload, and manual rebuild progress;
  `verbose` also reports analysis-setting diagnostic refreshes. `off` hides
  status progress but retains lifecycle and error details in the client log.
- `schemaPath` points to a custom schema JSON file. Unreadable or invalid files
  produce a warning and fall back to the built-in schema.
- `analysis.modelMemberStrictness` is `off`, `compatible` (member exists in any
  applicable model), or `strict` (member exists in every applicable model). It
  defaults to `compatible`.
- `analysis.allowPercentagesWithoutSign` accepts engine-compatible bare numbers
  in percentage fields. It defaults to `false`, requiring the trailing `%`.
- `analysis.mapOrderingDiagnostics` controls source-backed forward-order
  warnings in `map.ini` and `solo.ini`. It defaults to `true`.
- `analysis.debounceMs` waits this many milliseconds after the latest edit
  before refreshing whole-document indexes and diagnostics. Parsing and
  completions remain immediate. It defaults to `250`; valid values are
  `0`–`5000`, where `0` refreshes as soon as possible.
- `baseIniRoots` accepts directories and `.big` archives containing base game
  or mod INI files and game assets. WAV/MP3 filenames and TGA/DDS textures power
  asset completion and warnings; DDS-only textures complete as the canonical
  INI spelling `stem.tga`. Audio and texture warnings activate independently
  only after that asset kind is indexed. Supply every loaded game/mod root to
  avoid warnings caused by a partial asset index. INI definitions are treated
  as loaded before `map.ini` and `solo.ini`. Definitions in loose files open
  through native `file:` URIs. Definitions inside `.big` archives open as
  read-only virtual INIs with navigation and inspection support when the client
  selects the `big` URI scheme.

The same settings can be sent at runtime through
`workspace/didChangeConfiguration`, either directly or nested under
`{"zerosyntax": ...}`. Analysis, debounce, and formatting changes apply
immediately. Schema and base-root changes rebuild the complete index, keep
filesystem scanning on a blocking worker, and report progress through file
discovery, cache checking, index activation, and open-document diagnostics.
Identical settings are ignored.

Clients without dynamic formatting registration keep their startup formatting
capability. If such a client starts with formatting disabled, it must restart
to expose formatting; all other settings still hot-reload. Only selecting a
different server executable inherently requires a new process.

## Persistent index cache

Indexing results are cached on disk so a restart reuses unchanged files. The
cache lives in `%LOCALAPPDATA%\zerosyntax` on Windows and
`$XDG_CACHE_HOME/zerosyntax` (else the temp directory) elsewhere, in one
versioned SQLite database. Records belong to canonical physical inputs rather
than a workspace or configured root set. Consequently, changing workspaces,
adding or reordering `baseIniRoots`, or changing a root's base/workspace role
reuses every unchanged input and parses only new or modified files. A BIG
archive is one physical input whose cached payload contains its virtual files.

Record identity includes the input fingerprint, the actual configured schema,
and the scanner/extractor format. SQLite WAL transactions allow editor windows
to share the database safely; cache failures degrade to ordinary parsing, a
corrupt payload invalidates only its input, and a corrupt database is rebuilt.
Failed scans are never cached. Clear/rebuild increments a database epoch so an
older in-flight scan cannot repopulate data after the clear completes.

Unused records expire after 30 days and the logical payload budget is 1 GiB,
with least-recently-used records removed first. Older root-set JSON caches are
removed after the new store commits successfully. Deleting the cache directory
by hand is safe while the language server is stopped — the next scan rebuilds
it.

## Supported LSP features

ZeroSyntax supports incremental document sync, diagnostics, completion, hover,
go to definition, references, rename, semantic tokens, document and workspace
symbols, folding ranges, quick fixes, and optional document formatting.

`Animation` and `IdleAnimation` offer qualified W3D animation names matching the
`Model` in the current condition state. States without a `Model` inherit it from
the preceding `DefaultConditionState` in the same draw module; `TransitionState`
works the same way. Matching uses the model's skeleton and animation headers,
including animations stored in separate loose W3D files or BIG archives under
workspace roots or `baseIniRoots`. An explicit `Model = None` offers no animations.
Suggestions apply only to the first value, preserving optional distance and repeat
arguments. Semantic highlighting treats animation names (including quoted names)
as references and distance/repeat arguments as numbers. Header layouts and state inheritance follow the
[engine source](https://github.com/electronicarts/CnC_Generals_Zero_Hour/blob/main/GeneralsMD/Code/GameEngineDevice/Source/W3DDevice/GameClient/Drawable/Draw/W3DModelDraw.cpp)
and [W3D format definitions](https://github.com/electronicarts/CnC_Generals_Zero_Hour/blob/main/GeneralsMD/Code/Libraries/Source/WWVegas/WW3D2/w3d_file.h).

W3D model completion items support `completionItem/resolve`. Clients that render
Markdown completion documentation can show a lazy textured thumbnail
for the active `Model =` suggestion. The initial completion list contains no
image data. Previews use indexed loose or BIG-contained W3D/TGA/DDS assets and
cover mesh geometry, base materials, HLOD composition, and hierarchy bind pose.
`preview.imageWidth` changes its displayed size and `preview.zoomPercent`
changes the model framing.
Malformed or unsupported assets leave completion functional and show a short
preview-unavailable message. Rebuild the asset index or reload the server after
changing binary assets.

## Build from source

Install Rust 1.75 or newer, clone the repository, and run:

```sh
cargo build --locked --release -p zerosyntax-server
```

The binary is written to `target/release/zerosyntax-lsp` (`.exe` on Windows).
