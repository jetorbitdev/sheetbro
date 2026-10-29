# SheetBro

A fast, native spreadsheet app for macOS that your AI assistant can work in.

SheetBro opens and saves Excel (`.xlsx`) and CSV files, calculates formulas with
[IronCalc](https://github.com/ironcalc/IronCalc), and runs a built-in
[MCP](https://modelcontextprotocol.io) server so tools like Claude, Cursor or Codex can read
and edit the workbook you have open. You can see every change they make as it happens, and
undo it with <kbd>⌘Z</kbd>.

![SheetBro with a Q3 sales report open, next to a Claude Code session that adds a revenue-by-region summary to the sheet through SheetBro's MCP tools](docs/screenshot.png)

## Features

- **Excel and CSV files.** Open `.xlsx`, `.xlsm`, `.csv` and `.tsv` files. Save as `.xlsx`, or
  export one sheet as `.csv`. You can also drag a file onto the window or open it from Finder.
- **Formulas.** Almost 500 Excel functions, grouped by category in a function menu with
  documentation. While you type a formula you get a live preview, and clicking cells inserts
  references. AutoSum works too.
- **Formatting.** Bold, italic, underline, text and fill colors, alignment, currency, percent
  and decimal places, and merged cells.
- **Familiar editing.** Keyboard navigation (arrow keys, <kbd>⌘</kbd>+arrow, Tab/Enter, F2),
  copy and paste with other spreadsheet apps, column resizing and auto-fit, multiple sheets,
  and full undo/redo.
- **Selection stats.** Sum, average and count of the selected range.
- **AI tools over MCP.** Connect almost any MCP client. See below.

## Connecting AI tools

1. Open the **AI tools** panel in SheetBro and turn on **Allow AI tools to connect**.
2. Pick your client. SheetBro shows a ready-made config snippet for Claude Code, Claude Desktop,
   Cursor, VS Code, Windsurf, Gemini CLI and Codex CLI.
3. Copy the snippet into your client's MCP settings.

![The AI tools panel with the MCP server turned on and the Claude Code setup command shown](docs/mcp-panel.png)

There are two ways to connect:

| Transport | Used by | How it works |
|---|---|---|
| Streamable HTTP | Claude Code, Cursor, VS Code, Windsurf, Gemini CLI | `POST http://127.0.0.1:7424/mcp` with an `Authorization: Bearer <token>` header |
| stdio | Claude Desktop, Codex CLI | The client launches the SheetBro binary with `--mcp-stdio`, which relays messages to the running app |

For example, with Claude Code:

```sh
claude mcp add --transport http sheetbro http://127.0.0.1:7424/mcp \
  --header "Authorization: Bearer <token>"
```

### Available tools

| Tool | What it does |
|---|---|
| `workbook_info` | File name, path, unsaved-changes flag, and the sheets with their used size |
| `read_range` | Reads cells as displayed text, or as formulas |
| `write_range` | Writes a grid of values and formulas, and reports any formula errors |
| `clear_range` | Clears cell contents and keeps formatting |
| `format_range` | Bold, italic, underline, colors, alignment, number format |
| `add_sheet` | Adds an empty sheet |
| `undo` / `redo` | Undoes or redoes the last edit, whether it was made by you or the AI |
| `open_workbook` | Opens a file. It refuses to discard unsaved changes unless asked to. |
| `save_workbook` | Saves the file, or saves to a new path. It won't overwrite an existing file unless asked to. |

### Security

- The server is **off by default**. When it is on, it listens only on `127.0.0.1`.
- HTTP requests need a random access token. You can generate a new one from the panel, and
  clients using the old token are then locked out.
- Settings are stored in `mcp.json` inside a `com.jetorbit.spritsit` folder in your config
  directory: `~/Library/Application Support` on macOS, `%APPDATA%` on Windows, and
  `~/.config` on Linux.
- Every AI edit goes into the normal undo history.

## Building from source

SheetBro is built with [Tauri 2](https://tauri.app). It is developed and tested on macOS.
Windows and Linux builds work, but they get less testing, and opening a file by
double-clicking it in Explorer or a file manager isn't handled yet.

On every OS you need:

- [Rust](https://rustup.rs) (stable)
- [Node.js](https://nodejs.org) 20.19+ or 22.12+ and [pnpm](https://pnpm.io)

Then install the OS-specific requirements below, and get the source:

```sh
git clone https://github.com/jetorbitdev/sheetbro.git
cd sheetbro
pnpm install
pnpm tauri dev      # run in development mode
```

`tauri.conf.json` only lists the macOS bundle formats, so on Windows and Linux pass
`--bundles` to choose an installer format. All bundles are written to
`src-tauri/target/release/bundle/`.

### macOS

Requirements:

- macOS 11 or later
- Xcode Command Line Tools (`xcode-select --install`)

```sh
pnpm tauri build    # SheetBro.app in bundle/macos, a .dmg in bundle/dmg
```

### Windows

Requirements:

- Windows 10 or 11 (64-bit)
- [Microsoft C++ Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/)
  with the **Desktop development with C++** workload
- [WebView2](https://developer.microsoft.com/microsoft-edge/webview2/). It is already
  installed on Windows 11 and up-to-date Windows 10.
- The MSVC Rust toolchain: `rustup default stable-msvc`

In PowerShell:

```powershell
pnpm tauri build --bundles nsis   # setup .exe in bundle\nsis
pnpm tauri build --bundles msi    # .msi in bundle\msi (downloads the WiX toolset on first run)
```

The plain executable is at `src-tauri\target\release\spritsit.exe`.

### Linux

Install the WebKitGTK and build dependencies for your distribution.

Debian / Ubuntu (22.04 or later):

```sh
sudo apt update
sudo apt install libwebkit2gtk-4.1-dev build-essential curl wget file \
  libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev
```

Fedora:

```sh
sudo dnf install webkit2gtk4.1-devel openssl-devel curl wget file \
  libappindicator-gtk3-devel librsvg2-devel libxdo-devel
sudo dnf group install "c-development"
```

Arch:

```sh
sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl \
  appmenu-gtk-module libappindicator-gtk3 librsvg xdotool
```

Then build:

```sh
pnpm tauri build --bundles deb,appimage   # .deb in bundle/deb, .AppImage in bundle/appimage
pnpm tauri build --bundles rpm            # .rpm in bundle/rpm (Fedora, openSUSE)
```

The plain executable is at `src-tauri/target/release/spritsit`.

## Project structure

```
src/                 React + TypeScript frontend
  App.tsx            toolbar, menus, keyboard handling, selection
  Grid.tsx           virtualized cell grid and in-cell editor
  McpPanel.tsx       "AI tools" settings panel
  functionList.ts    generated list of supported functions
  functionDocs.ts    function descriptions for the function menu
src-tauri/
  src/lib.rs         workbook model, file I/O, Tauri commands, app menu
  src/mcp.rs         MCP server (HTTP + stdio bridge) and tools
scripts/
  gen_functions.py   regenerates src/functionList.ts from IronCalc
```

After you bump the `ironcalc` dependency, regenerate the function list:

```sh
cd src-tauri && cargo fetch && cd ..
python3 scripts/gen_functions.py
```

## Changelog

Each release below is also published as a
[GitHub release](https://github.com/jetorbitdev/sheetbro/releases), which is where the
changelog page on the SheetBro website gets its entries. To publish one, use the version
as the tag, the title as the release name, and paste the bullet list as the release notes.
Keep one change per line and start it with `feat:`, `fix:`, `perf:` or `docs:`, so the website
can label it New, Fix, Perf or Docs.

### v0.1.0 — First public release

_Unreleased_

- feat: Open .xlsx, .xlsm, .csv and .tsv files, by dialog, drag and drop, or from Finder
- feat: Save as .xlsx, and export a sheet as .csv
- feat: Almost 500 Excel functions with a function menu, live formula preview and AutoSum
- feat: Bold, italic, underline, text and fill colors, alignment, number formats and merged cells
- feat: Keyboard navigation, copy and paste with other spreadsheet apps, column auto-fit and multiple sheets
- feat: Each workbook opens in its own window; on macOS the app stays in the Dock when the last one closes
- feat: Sum, average and count of the selected range
- feat: Built-in MCP server over Streamable HTTP and stdio, with ready-made setup for Claude, Cursor, VS Code, Windsurf, Gemini CLI and Codex CLI
- feat: AI edits go into the normal undo history
- docs: Build instructions for macOS, Windows and Linux

## Contributing

Issues and pull requests are welcome. For larger changes, please open an issue first to
discuss what you'd like to change.

## License

[MIT](LICENSE). The bundled IBM Plex fonts are licensed under the
[SIL Open Font License 1.1](https://openfontlicense.org).
