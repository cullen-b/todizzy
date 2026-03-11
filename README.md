# Todizzy

A tiny menu-bar notepad for macOS. Click the icon, type, click away.

Built with pure Rust + AppKit — no Electron, no webview. ~1.1 MB universal binary.

---

## Features

- **Lives in your menu bar** — one click to open, Esc Esc to close
- **Multiple notes** — swipe or use ‹ › to flip between pages
- **Vim & Helix keybindings** — or turn them off and just type
- **Live markdown** — headings, bold, italic, inline code, blockquotes, strikethrough
- **Git sync** — pull on open, push on close (optional)
- **Plain `.txt` files** — no lock-in, stored in `~/Library/Application Support/todizzy/`

## Requirements

macOS 13+ · Apple Silicon & Intel

## Install

Download the latest release from the [releases page](https://github.com/cullen-b/todizzy/releases/latest), unzip, and drag `Todizzy.app` to `/Applications`.

**First launch:** macOS may block the app since it isn't notarized. Right-click → **Open** → **Open** to allow it. If that doesn't work, go to **System Settings → Privacy & Security** and click **Open Anyway**.

## Build from source

```bash
git clone https://github.com/cullen-b/todizzy.git
cd todizzy
make bundle   # builds Todizzy.app at target/release/Todizzy.app
make run      # build + open
```

```bash
cargo test    # run pure-Rust unit tests (no macOS required)
```

Set `TODIZZY_DATA_DIR=/tmp/todizzy-dev` to use a separate data directory during development.

## Git sync setup

1. Create a private GitHub repo
2. Init the notes folder: `cd ~/Library/Application\ Support/todizzy/notes && git init && git remote add origin <url> && git push -u origin main`
3. Enable **Git sync** in Todizzy settings (right-click the menu bar icon)

See the [website](https://cullen-b.github.io/todizzy#git-sync) for full instructions.

## License

MIT — see [LICENSE](LICENSE)
