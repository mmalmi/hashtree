# Iris Files

Content-addressed file storage on Nostr. A web app built on [hashtree](../hashtree).

## Features

- Content-addressed file storage with SHA256 merkle trees
- P2P file sync via WebRTC with Nostr signaling
- Mutable `npub/path` addresses via Nostr events
- Collaborative editing with Yjs CRDT
- Git-like version control (commits, branches)
- Cashu wallet integration
- Offline-first architecture

## URL Format

Routes use `/#/{npub}/{treeName}/{path...}` where treeName is URL-encoded (e.g. `my%20doc` for `my doc`).

## Web App

```bash
# Development
npm run dev

# Build
npm run build

# Preview build
npm run preview
```

## Desktop App (Tauri)

Build as a native desktop application with [Tauri](https://tauri.app/).

### Prerequisites

Install Tauri prerequisites for your platform: https://v2.tauri.app/start/prerequisites/

- **macOS**: Xcode Command Line Tools
- **Windows**: Microsoft Visual Studio C++ Build Tools, WebView2
- **Linux**: Various system dependencies (see Tauri docs)

Plus Rust: https://rustup.rs/

### Development

```bash
npm run tauri:dev
```

This starts the Vite dev server and opens a native window with hot reload.

### Build

```bash
npm run tauri:build
```

Outputs platform-specific installers in `src-tauri/target/release/bundle/`:
- **macOS**: `.dmg`, `.app`
- **Windows**: `.msi`, `.exe`
- **Linux**: `.deb`, `.AppImage`

### Desktop Features

- **Autostart**: Launch on login (toggle in Settings > Desktop App)
- **System tray**: Background operation with tray icon
- **Native dialogs**: File open/save dialogs
- **Notifications**: Native OS notifications

### Bundling hashtree-cli

To include the `htree` CLI tool in the desktop app:

1. Build htree for target platforms:
   ```bash
   cd ../../rust  # from apps/iris-files
   cargo build --release -p hashtree-cli
   ```

2. Create `src-tauri/bin/` and add platform-specific binaries:
   ```
   src-tauri/bin/
   ├── htree-x86_64-pc-windows-msvc.exe
   ├── htree-x86_64-apple-darwin
   ├── htree-aarch64-apple-darwin
   └── htree-x86_64-unknown-linux-gnu
   ```

3. Update `src-tauri/tauri.conf.json`:
   ```json
   "externalBin": ["bin/htree"]
   ```

4. Access from frontend via Tauri's shell API.

## License

MIT
