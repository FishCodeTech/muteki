# Muteki desktop

This Rust executable hosts the original `apps/frontend` React/Next UI inside an
NSH `WKWebView`, with NSH's native application menus and window lifecycle. The
business API runs in the separate Rust `muteki-server` process.

From the repository root:

```sh
node scripts/dev.mjs
node scripts/dev.mjs --web
node scripts/package-macos.mjs
```

The development script builds the Rust executables and the Next production UI.
Use `--skip-build` to reuse those existing builds. The desktop checks backend
identity at port 8100 and the frontend HTML plus its API health proxy at port
3101. It reuses verified services and terminates only children it started.
Services listen on `127.0.0.1`. `MUTEKI_UI_ORIGIN` can select another local HTTP
port. `MUTEKI_NODE` selects the development Node executable.

The packaged `dist/Muteki.app` contains the NSH executable, Rust API executable,
Next standalone server, static assets, public assets, and official Node 22.23.1.
The packaging script downloads the architecture-specific Node archive from
nodejs.org, verifies its SHA256 against that release's official checksums, and
caches it under `.cache/node-runtime`. Node serves the frontend only. Installed
apps resolve all runtime resources relative to their bundle and do not need
Node on the user's PATH or the source checkout.

Development data defaults to repository `.state`; `MUTEKI_STATE_DIR` overrides it.
Installed Muteki uses `~/Library/Application Support/Muteki/data`, independent of
the installation directory. When an old Electron stable environment exists,
first launch preserves its complete tree under `Muteki/migration-backups`, verifies
its contents, and imports configuration, historical records and verified CHAT
attachments before starting the service. Running legacy backends block migration.
Interrupted imports retry on the next launch; sources and backups are retained.
Historical executions remain interrupted, rather than automatically running again.
Development environments are not merged into the installed stable environment.
Without a legacy stable environment, an existing `Muteki Rust/muteki.sqlite` is
adopted through a directory link, retaining absolute paths and Runner mounts.
`MUTEKI_STATE_DIR` bypasses automatic discovery and migration.

The packaged frontend runs from `~/Library/Caches/Muteki/frontend/<build-id>` so
Next cache writes never touch the signed application. Installed services use
independent loopback ports, persisted in `desktop/service-ports.json` to preserve
WebKit storage and draft origins across restarts, and verify storage identity before reuse. Startup
and migration diagnostics are private files inside the data directory.

`node scripts/install-macos.mjs [PATH/Muteki.app]` verifies a local package,
prepares storage, retains the complete previous App (including embedded legacy
files), and replaces `/Applications/Muteki.app`. Quit the installed App first.
This does not publish a release. `Muteki --muteki-prepare-storage` performs only
the storage preparation, without opening a window.

Release profiles and zip archives require the pinned Developer ID Application identity (Team H53Q22J4T3). Local/debug bundles without zip may use ad-hoc; `--identity` / `MUTEKI_CODESIGN_IDENTITY` selects an explicit identity. It verifies the complete bundle. Signing does not notarize or publish the application. See [development instructions](../../docs/development.md).

## macOS Bundle `Info.plist` Keys

For custom `muteki://` deep-link routing and native speech/microphone permissions,
the packaged `Muteki.app/Contents/Info.plist` should include:

```xml
<key>CFBundleURLTypes</key>
<array>
  <dict>
    <key>CFBundleURLName</key>
    <string>tech.fishcode.muteki.desktop</string>
    <key>CFBundleURLSchemes</key>
    <array>
      <string>muteki</string>
    </array>
  </dict>
</array>
<key>NSMicrophoneUsageDescription</key>
<string>Muteki 需要访问麦克风以进行语音输入。</string>
<key>NSSpeechRecognitionUsageDescription</key>
<string>Muteki 需要语音识别权限以将您的语音转换为文字。</string>
```

## 桌面前端热更新

```sh
node scripts/dev.mjs --hot
```

打开连接 `http://127.0.0.1:3103` 的桌面开发窗口，TSX/CSS 保存后由 Next Fast Refresh 更新。首次进入页面会按需编译；不需要 `next build` 或重新打包 `.app`。开发输出独立保存在 `apps/frontend/.next-dev`，不会覆盖发布用的 `.next`。

该模式优先复用已有 debug Rust 程序，否则使用 release；仅在程序缺失时构建 Rust。`--release` 可显式选择 release，`--skip-build` 禁止自动构建。修改 Rust 后仍需重新构建并重启开发窗口。`node scripts/dev.mjs --hot --web` 仅启动浏览器预览服务。

保留命令运行即可持续预览；Ctrl+C 停止本次启动的窗口和服务，已在运行的后端会保留。预览端口被占用时先停止上一次预览。开发窗口与正式应用使用同一份本机数据；需要隔离时设置 `MUTEKI_STATE_DIR` 并使用独立后端。现有正式窗口不会自动切换到开发服务，首次使用此命令打开开发窗口后，日常前端修改无需重启窗口。
