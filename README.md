<h1 align="center">ParquetView</h1>

<p align="center">A fast, native Parquet viewer for macOS.</p>

<p align="center">
  <a href="https://github.com/Alyetama/ParquetView/releases/latest/download/ParquetView.dmg"><b>⬇️ Download for macOS</b></a>
  &nbsp;·&nbsp;
  <a href="https://alyetama.github.io/ParquetView">Website</a>
  &nbsp;·&nbsp;
  <a href="#first-launch">First launch</a>
</p>

<p align="center">
  <img src="docs/mockup.png" alt="ParquetView showing a Parquet file" width="820">
</p>

Parquet files are annoying to peek into. You either fire up Python and pandas, or hunt for some web tool you don't trust with your data. ParquetView is just a Mac app: double-click a `.parquet` file and look at it.

It only reads the rows that are on screen, so a multi-gigabyte file opens as fast as a small one.

## Download

**[⬇️ Download ParquetView for macOS](https://github.com/Alyetama/ParquetView/releases/latest/download/ParquetView.dmg)** (Apple Silicon & Intel)

Open the `.dmg`, drag ParquetView to Applications, then see [First launch](#first-launch) below (it's unsigned, so macOS needs one extra click).

## Features

- Opens huge files without loading them into memory. It reads row groups as you scroll.
- Open a file however you want: the file picker, drag-and-drop, or Finder's "Open With".
- Shows the schema and column types up front, plus a metadata panel with row count, file size, compression codec, row-group count, and the writer.
- Click a column header to sort it.
- Search across everything, or build a proper filter with multiple conditions (contains, equals, regex, `>`, `<`, is-empty) joined by AND/OR.
- Double-click a cell to copy or edit its value.
- Light/dark/auto theme, row density, and font size. It remembers what you picked.

## First launch

ParquetView isn't signed with an Apple Developer ID, so macOS blocks it the first time you open it. Nothing's wrong; you just have to tell macOS you meant it. Pick one:

1. **Right-click to open.** In Finder, right-click (or Control-click) ParquetView, choose **Open**, then **Open** again.
2. **On newer macOS,** if that doesn't offer an Open button: go to **System Settings → Privacy & Security**, scroll down, and click **Open Anyway**.
3. **Or from the Terminal,** strip the quarantine flag and open it normally:
   ```bash
   /usr/bin/xattr -dr com.apple.quarantine /Applications/ParquetView.app
   ```

## Build from source

You'll need Rust, Node 18+, and the Xcode command-line tools.

```bash
npm install
npm run tauri build   # → src-tauri/target/release/bundle/macos/ParquetView.app
```

`npm run tauri dev` runs it with hot reload.

It's a [Tauri](https://tauri.app) app: a Rust backend with a web frontend, and the `arrow`/`parquet` crates do the actual Parquet reading.

## License

[MIT](LICENSE) © 2026 Alyetama
