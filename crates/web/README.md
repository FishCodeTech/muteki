# Rust web and macOS runners

The application UI, state, and handlers live in `crates/ui` and run in Rust on both targets. The macOS app mounts `muteki_ui::MutekiApp` directly in NSH's native AppKit backend. The web library mounts that same root with `nsh_platform_web::rust_app!` and `default-features = false`.

The web page loads upstream NSH's `nsh.js`, the DOM adapter for the Rust renderer. It does not load the original React/Next app, a TSX bundle, or the browser JavaScript app host. `scripts/build-web.mjs` is build and local serving tooling.

Run these commands from the repository root. Rust and `sccache` must be on `PATH`; install the web target once:

```sh
rustup target add wasm32-unknown-unknown
```

Start the native macOS app and its backend:

```sh
node scripts/dev.mjs
```

The app verifies `/api/health` identifies `muteki-rust` before reusing a service on port 8100. Otherwise it starts its sibling `muteki-server`, waits for its real HTTP health response, and stops only that owned process on application exit. A different service on port 8100 produces an explicit startup error.

For manual startup:

```sh
cargo build -p muteki-desktop -p muteki-server
cargo run -p muteki-desktop
```

Build and serve the web app:

```sh
node scripts/dev.mjs --web
```

Open `http://127.0.0.1:3101`. The local server proxies `/api` and `/health` to `http://127.0.0.1:8100`. `PORT` changes the web port; `MUTEKI_API_ORIGIN` changes the local proxy destination.

Build a static web package without serving:

```sh
node scripts/build-web.mjs
```

Output is `dist/web`. Serve an existing build without recompiling:

```sh
node scripts/build-web.mjs --serve-only
```

For a native release executable:

```sh
cargo build -p muteki-desktop --release
```

The executable is `.cache/target/release/muteki-desktop`. Build `muteki-server` alongside it as well, or use the package command:

```sh
node scripts/package-macos.mjs
open dist/Muteki.app
```

This creates an unsigned local `.app` containing both Rust binaries, registered UI assets, and the original product icon. It does not publish, upload, or sign the app. To package binaries already built by another build step, use `--skip-build`; add `--debug` to package the debug binaries.

Bundled apps read their packaged `Contents/Resources/muteki-assets` and do not depend on source asset paths. Development runs stage registered images and fonts alongside the executable. Development state lives in the workspace `.state`; the bundled app defaults to `~/Library/Application Support/Muteki`. Set `MUTEKI_STATE_DIR` to choose a different state directory for either runner. A reused running service retains its own state directory.

The web build uses the shared `.cache/target` and `.cache/sccache`, the workspace `web` profile, NSH's custom entropy backend, and an 8 MiB Wasm stack. It exports registered assets through the desktop CLI, then compiles the web crate. For static deployment, route API requests to the Rust service on the same origin; when building for a different API origin, set `MUTEKI_API_URL` at compile time.
