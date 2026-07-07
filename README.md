# Flutter Overlay D3D11

Host a Flutter app on Windows from your own Rust process. You link this crate,
load the Flutter engine at runtime, and drive it yourself. Two ways to use it:

- **Embed into a D3D11 app or game (main use case).** Render Flutter into a
  Direct3D 11 texture that your app or a game you hook already owns, and
  composite it over your scene. You call into the crate from your present hook,
  your `WndProc`, and your resize hook. On top of the 2D UI you can also draw 3D
  primitives, 3D text, and custom pixel shaders into the world.
- **Standalone window (secondary).** One blocking call opens a normal Flutter
  window in its own HWND.

Rendering is OpenGL via ANGLE (D3D11-backed, frames shared GPU-to-GPU through a
keyed mutex, no CPU copy), with an automatic CPU software fallback when ANGLE is
unavailable.

Full API reference: `cargo doc --no-deps --open`. The embed API lives on the
`software_renderer::overlays_manager_api` page.

> Note on the name: the `software_renderer` module is named for historical
> reasons (the CPU software renderer was implemented first). It is **not**
> software-only. It holds both rendering paths, including the ANGLE
> hardware-accelerated one, and is the D3D11 embedder as a whole.

---

## Prerequisites

- Rust, stable toolchain.
- Windows 10 SDK (for the `windows` crate).
- Flutter **3.35.7** desktop. The engine DLLs you ship must match this version.

---

## Step 1: Get the Flutter runtime files

Two kinds of files end up in the same folder: the **app bundle** you build, and
the **engine runtime** you obtain separately.

### The engine runtime

Three files alongside your bundle come from outside `flutter assemble`:

```
flutter_engine.dll              <- Flutter engine (mode-dependent, see below)
libEGL.dll   libGLESv2.dll      <- ANGLE
```

(`icudtl.dat` is the fourth runtime file, but you do not fetch it separately:
it ships with every Flutter install and is copied into the bundle by the build,
both `flutter build windows` and `flutter assemble`.)

**`flutter_engine.dll`**, by build mode:

- **Debug / JIT**: download the prebuilt embedder zip from Google. With
  `engine_version` taken from `<flutter_sdk>/bin/internal/engine.version`:

  ```
  https://storage.googleapis.com/flutter_infra_release/flutter/{engine_version}/windows-x64/windows-x64-embedder.zip
  ```

  Unzip and take `flutter_engine.dll`.

- **Release / AOT**: there is **no prebuilt release `flutter_engine.dll`**. You
  must compile it yourself from the Flutter engine repo
  (<https://github.com/flutter/engine>), at the version that matches this crate's
  bindings (currently **3.35.7**, unless you regenerate the bindings). That AOT
  `flutter_engine.dll` is all you need; the app's AOT code is just `app.so` from
  Step 2 (no `gen_snapshot`, no separate snapshot step).

**ANGLE files** (`libEGL.dll`, `libGLESv2.dll`) are **not** Flutter artifacts;
they are ANGLE, which enables the GPU path. Get them either by copying the two
DLLs from a Chrome / Chromium install, or by building them from the ANGLE repo
(<https://github.com/google/angle>). They are version-independent of Flutter.
Without them the crate falls back to the software renderer.

### The app bundle (`flutter assemble`)

Do **not** use `flutter build windows`; that builds the C++ runner you are
replacing. Use `flutter assemble`, which emits just the assets plus the Dart
artifact.

Release (AOT):

```bash
flutter pub get
flutter assemble --output=build -dTargetPlatform=windows-x64 -dBuildMode=release release_bundle_windows-x64_assets
```

Debug (JIT):

```bash
flutter assemble --output=build -dTargetPlatform=windows-x64 -dBuildMode=debug debug_bundle_windows-x64_assets
```

### Final layout

Put the engine runtime next to the assembled bundle. For the embedder path this
is the complete set; nothing else is needed:

```
my_overlay/Release/          <- the folder you point the crate at
├── flutter_assets/          <- from assemble
├── icudtl.dat               <- from assemble (ships with Flutter)
├── app.so                   <- from assemble (release/AOT; debug uses kernel_blob.bin)
├── flutter_engine.dll       <- Flutter engine (JIT: Google zip; AOT: compiled yourself)
├── libEGL.dll               <- ANGLE (from Chrome or the ANGLE repo)
└── libGLESv2.dll            <- ANGLE
```

Release uses `app.so`; debug uses `kernel_blob.bin`.

The D3D11 embedder does **not** load Flutter plugins, so no plugin `*.dll`s go
here. (Plugins and `*.dll` discovery are a standalone-only feature; the
standalone path also uses `flutter_windows.dll` instead of `flutter_engine.dll`.
See the standalone section below.)

---

## Step 2: Add the crate

```toml
[dependencies]
flutter_rust_windows_embedder = { git = "https://github.com/Vluurie/flutter-rust-windows-embedder.git", branch = "master" }
```

---

## Step 3: Embed into your D3D11 app

You already own an `ID3D11Device`, an `ID3D11DeviceContext`, and an
`IDXGISwapChain` (your renderer, or a game you inject into). You pass those real
objects straight in; no proxy or dummy device is created.

For a complete, runnable reference that stands up a real D3D11 device and swap
chain and drives `init_instance` end to end, see the test harness:
[`src/software_renderer/multiview/tests/harness.rs`](src/software_renderer/multiview/tests/harness.rs)
(built under the `engine-tests` feature; the `multiview/tests` folder around it
exercises the full setup, resize, and shutdown flow). The test Flutter app it
builds lives at
[`flutter_artifacts/test_libs/test_app`](flutter_artifacts/test_libs/test_app);
its README explains where to drop the engine artifacts (version **3.35.7**),
which are not committed.

The manager is a global handle you fetch with `get_flutter_overlay_manager_handle()`.
You can run several overlays at once; methods take an `identifier: Option<&str>`
where `None` targets the single active overlay.

There are four call sites. The snippets below are generalized from a real
present-hook integration.

### 1a. Preload the runtime DLLs early (before any hooks)

Call `preload_flutter_runtime_dlls` once, **early in your own DLL/mod
initialization, before you install the present/WndProc/resize hooks**, not from
inside the render thread. It loads `flutter_engine.dll` and the ANGLE DLLs up
front. Loading those DLLs runs their `DllMain` and sets up thread-local state; if
that happens lazily on the render thread at the same moment the engine is
spinning up, it can race. Doing it once, ahead of time, on your init thread keeps
that out of the hot path so `init_instance` cannot race the DLL load.

```rust
use std::path::Path;
use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::preload_flutter_runtime_dlls;

// In your mod/DLL entry point, before installing hooks:
fn early_init() {
    let overlay_dir = Path::new(r"C:\path\to\my_overlay\Release");
    preload_flutter_runtime_dlls(overlay_dir); // idempotent; loads engine + ANGLE DLLs
}
```

### 1b. Create the overlay once, on first present

```rust
use std::path::Path;
use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::get_flutter_overlay_manager_handle;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;

fn init_overlay(swap_chain: &IDXGISwapChain) {
    // The Release (or Debug) folder from Step 1 (already preloaded in 1a).
    let overlay_dir = Path::new(r"C:\path\to\my_overlay\Release");

    let Some(om) = get_flutter_overlay_manager_handle() else { return };

    // Dart entrypoint args: arrive in Dart's `main(List<String> args)`. For
    // example a native callback address Dart can call when it is ready.
    let dart_args = vec![
        format!("--notify-ready-fn={}", on_dart_ready as *const () as usize),
    ];

    // Engine args (the C engine's own switches). Useful in debug/JIT, e.g.:
    //   "--vm-service-port=9501", "--disable-service-auth-codes",
    //   "--verbose-system-logs", "--trace-skia"
    // Pass None for release/AOT.
    let engine_args: Option<Vec<String>> = None;

    let ok = om.init_instance(
        swap_chain,
        overlay_dir,
        "main_ui",        // instance identifier
        Some(dart_args),  // or None
        engine_args,
    );
    if !ok {
        // engine failed to start; check the runtime DLLs and bundle layout
    }
}

extern "C" fn on_dart_ready() { /* Dart calls this once initialized */ }
```

Call this exactly once, guarded (e.g. `std::sync::Once`), the first time your
present hook runs with a valid swap chain.

### 2. Render every frame, inside your present hook

Order matters. Latch, draw world-space 3D primitives against the game's depth
buffer (before any post-processing that flattens the target), then composite the
UI.

```rust
use directx_math::XMMatrix;
use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::get_flutter_overlay_manager_handle;
use windows::Win32::Graphics::Direct3D11::ID3D11DepthStencilView;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;

fn render_overlay(
    swap_chain: &IDXGISwapChain,
    view_proj: XMMatrix,                  // your camera's view*projection
    game_dsv: &Option<ID3D11DepthStencilView>,
) {
    let Some(om) = get_flutter_overlay_manager_handle() else { return };

    if om.should_attempt_recovery() {
        om.attempt_device_recovery(swap_chain);
    }

    om.latch_all_queued_primitives();
    om.latch_all_queued_text();

    om.render_primitives(&view_proj, game_dsv);

    // ... your post-processing here, if any ...

    om.render_ui();
}
```

### 3. Forward input, inside your WndProc

Give Flutter the message first. If it consumes the message, do not pass it on to
the game.

```rust
use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::get_flutter_overlay_manager_handle;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};

fn on_message(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if let Some(om) = get_flutter_overlay_manager_handle() {
        if om.forward_input_to_flutter(hwnd, msg, wparam, lparam) {
            return Some(LRESULT(1)); // consumed by Flutter
        }
    }
    None // let the original WndProc / game handle it
}
```

### 4. Resize, inside your ResizeBuffers hook

```rust
use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::get_flutter_overlay_manager_handle;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

fn on_resize(swap_chain: &IDXGISwapChain, width: u32, height: u32) {
    let Some(om) = get_flutter_overlay_manager_handle() else { return };

    // Top-left of the output window, so overlays track the window position.
    let (mut x, mut y) = (0, 0);
    if let Ok(desc) = unsafe { swap_chain.GetDesc() } {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(desc.OutputWindow, &mut rect).is_ok() } {
            x = rect.left;
            y = rect.top;
        }
    }
    om.resize_flutter_overlays(swap_chain, x, y, width, height);
}
```

That is the full loop: your Flutter UI is now composited over your scene.

### Talking to Dart

**Startup args.** Arguments passed to `init_instance` arrive in the Dart
entrypoint, so native and Dart can hand each other startup data:

```dart
Future<void> main(List<String> arguments) async {
  WidgetsFlutterBinding.ensureInitialized();
  for (final arg in arguments) {
    if (arg.startsWith('--notify-ready-fn=')) {
      final ptr = int.tryParse(arg.substring('--notify-ready-fn='.length));
      // call back into native once initialized
    }
  }
  runApp(const MyApp());
}
```

**Native to Dart.** From native you can push values to Dart (`om.post_string`,
`om.post_bool`, ...), broadcast a platform-channel message
(`om.broadcast_platform_message`), or register a channel handler
(`om.register_channel_handler_for_instance`).

**Dart to native (flutter_rust_bridge).** For ongoing, typed calls in the other
direction, use [flutter_rust_bridge](https://github.com/fzyzcjy/flutter_rust_bridge)
(FRB). It generates the Dart <-> Rust bindings for you: you annotate Rust
functions and call them straight from Dart. On the Rust side:

```rust
use flutter_rust_bridge::frb;

#[frb(sync)]
pub fn set_world_collision_visualizer_enabled(enabled: bool) {
    // ... your native logic ...
}
```

FRB generates the matching Dart binding. You initialize the generated API once by
opening your Rust DLL, then call the functions directly:

```dart
// Init the generated bridge against your native DLL (once, at startup).
// (`MyApi` is the API class FRB generates from your crate.)
await MyApi.init(externalLibrary: ExternalLibrary.open(myDllPath));

// Later, anywhere in Dart, just call the generated function:
setWorldCollisionVisualizerEnabled(enabled: true);
final rect = gameScreenRect(); // a #[frb(sync)] fn that returns data
```

See the [flutter_rust_bridge documentation](https://cjycode.com/flutter_rust_bridge/)
for setup and codegen details.

> **Threading (important).** An FRB call from Flutter runs on **a random Flutter
> worker thread**, not your game/render thread. Touching shared state that the
> render thread also uses, or calling into the game, directly from an FRB
> function is undefined behavior / a data race. Guard plain shared state behind
> its own `Mutex`, and for anything that must run on the game/render thread,
> **queue it** and drain that queue from your per-frame hook (Step 3.2). In the
> reference code this is a `queue_main(move || { ... })` helper that defers the
> closure onto the game-thread task queue:
>
> ```rust
> #[frb(sync)]
> pub fn terminate_spawned_entity(handle_id: u32) {
>     // Game-thread work is deferred, not run on the FRB thread:
>     queue_main(move || {
>         // ... call into the game safely on its own thread ...
>     });
>     // Pure shared state guarded by its own Mutex is fine to touch directly:
>     SPAWNED_ENTITIES.lock().retain(|e| e.handle_id != handle_id);
> }
> ```

### Driving Dart state per frame

The overlay needs to re-pull native state every frame (entity lists, the game
screen rect, ...). The pattern used in the main overlay: a global frame runner
self-reschedules a post-frame callback, bumping a tick signal each frame, and
tick-dependent state reads that signal so it recomputes once per frame.

```dart
// 1. A global tick signal, bumped once per frame.
final gameTickSignal = Signal(0);
void tick() => gameTickSignal.value++;

// 2. A runner that calls tick() every frame by re-arming a post-frame callback.
class GlobalFrameRunner {
  void start() {
    SchedulerBinding.instance.addPostFrameCallback((_) => _runTick());
  }
  void _runTick() {
    tick();
    SchedulerBinding.instance.addPostFrameCallback((_) => _runTick());
  }
}
// GlobalFrameRunner().start(); // once, after the bridge is initialized

// 3. Tick-driven state: read gameTickSignal to subscribe, then pull from native.
//    `gameScreenRect()` is an FRB call into Rust; this recomputes every frame.
final getGameScreenRectComputed = Computed<ScreenRect?>(() {
  gameTickSignal.value;     // subscribe: re-run when the tick changes
  return gameScreenRect();  // FRB call -> fresh native data
});
```

Widgets then `Watch` those computed signals and rebuild each frame as native
state changes. (This uses the `signals` package; any reactive state library
works.)

## Multiple windows (multi-view)

One Flutter engine (one isolate, shared Dart state) can drive more than the
in-game overlay. You can spawn extra top-level OS windows, each backed by its own
Flutter view, rendered from the same app. This is great for tear-off panels: an
editor or inspector that pops out of the overlay into its own resizable window
while sharing all of the app's state. (Multi-view requires the OpenGL/ANGLE path.)

Spawn a window from native through the manager. You get a `SatelliteWindow` back;
keep it alive (dropping it closes the window), and use its `view_id` to address
the new view from Dart.

```rust
use flutter_rust_windows_embedder::software_renderer::overlays_manager_api::get_flutter_overlay_manager_handle;
use flutter_rust_windows_embedder::software_renderer::multiview::window::{WindowSpec, WindowStyle};

fn spawn_editor_window() -> Option<i64> {
    let om = get_flutter_overlay_manager_handle()?;
    let mut mgr = om.manager.lock();

    let spec = WindowSpec {
        title: "Script Editor".to_string(),
        width: 1100,
        height: 800,
        pixel_ratio: None,
        style: WindowStyle { decorated: true, resizable: true },
    };

    match mgr.spawn_window_for_overlay(None, spec) {
        Ok(window) => {
            let view_id = window.view_id() as i64;
            // Store `window` somewhere lasting (e.g. a Vec); dropping it closes the window.
            Some(view_id)
        }
        Err(_) => None,
    }
}
```

On the Dart side, render every live view with a `ViewCollection`, switching
content by `viewId` (0 is the main overlay; anything else is a spawned window):

```dart
@override
Widget build(BuildContext context) {
  final views = WidgetsBinding.instance.platformDispatcher.views;
  return ViewCollection(
    views: [
      for (final v in views)
        View(view: v, child: v.viewId == 0 ? const MainOverlay() : EditorWindow(viewId: v.viewId)),
    ],
  );
}
```

`SatelliteWindow` also exposes window controls (`minimize`, `maximize`, `restore`,
`start_drag`, `set_title`, `close`) so a custom, borderless Dart title bar can
drive the real OS window.

## Extra rendering APIs (optional)

Beyond compositing the Flutter UI, the same `om` handle exposes a small set of
in-world rendering helpers, in case you want to draw things in the game's 3D space
alongside the UI. These are optional extras, not the point of the crate, so they
are only summarized here; see the `FlutterOverlayManagerHandle` and
`d3d11_compositor` pages in `cargo doc` for full signatures and examples.

- **3D primitives**: submit a `Vec<Vertex3D>` (lines or triangles) under a named
  group and it draws every frame until cleared; `render_primitives` (Step 3.2)
  transforms it by your camera matrix.
  `om.set_primitives(None, "group", &vertices, PrimitiveType::Lines)`,
  `set_primitives_ex(..)` for depth/blend options, `clear_primitives(..)`. The
  `d3d11_compositor::primitive_presets` module builds boxes/spheres/cylinders.
- **3D text**: register a font atlas (`om.register_font_atlas(None, FontAtlasSpec
  { .. })`), build vertices with `text_presets::generate_text_vertices(..)`, then
  `om.set_text(..)`. Billboard helpers face text at the camera.
- **Custom pixel shaders**: register `.cso` bytecode with an optional constant
  buffer and texture slots (`om.register_custom_pixel_shader(..)`,
  `set_custom_effect_texture_at_slot(..)`, `update_custom_effect_constants(..)`),
  then draw with `om.set_custom_primitives(..)`.
- **Keybinds and visibility**: `om.register_visibility_toggle(..)`,
  `om.register_keybind_action(..)`, `om.set_visibility(..)`.

---

## Standalone window (secondary)

If you just want a Flutter window and do not need to composite it yourself:

```rust
use flutter_rust_windows_embedder::{init_flutter_window, init_flutter_window_from_dir};
use std::path::PathBuf;

fn main() {
    // Blocking: load the bundle next to this DLL/EXE and run a window.
    init_flutter_window();

    // Or point at a specific bundle (run off-thread so it does not block):
    std::thread::spawn(|| {
        let dir = PathBuf::from(r"C:\path\to\my_overlay\Release");
        init_flutter_window_from_dir(Some(dir)); // None -> fall back to DLL folder
    });
}
```

---

## License

- This crate is licensed under MIT. See [LICENSE](./LICENSE).
- Flutter engine and C API bindings are under the BSD 3-Clause license (see
  [LICENSE-THIRD-PARTY](./LICENSE-THIRD-PARTY)).
