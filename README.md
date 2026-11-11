# Flutter Overlay D3D11

> **Experimental.** This project changes constantly and is not maintained to
> stay stable. APIs, behaviour and requirements can break between any two
> commits. If you need a version that holds still, fork it and keep your own.

Run a Flutter app inside your own Rust process on Windows. The main use is
rendering it into a Direct3D 11 texture that your app, or a game you hook,
composites over its scene. It can also open a plain standalone Flutter window.

## Renderers

Picked automatically, each one falling back to the next:

1. **Vulkan** (Impeller or Skia). Needs only the GPU driver. Main overlay only,
   no extra windows yet.
2. **OpenGL via ANGLE**. Needs `libEGL.dll` and `libGLESv2.dll`. Required for
   extra windows.
3. **Software**. CPU fallback, Skia only.

Impeller is enabled with the engine argument `--enable-impeller=true`. To force
a renderer, call `set_renderer_preference` before `init_instance`.

## Requirements

- Windows, Rust (stable).
- Flutter **3.50.0**. `flutter_engine.dll` must match this version.

## Runtime files

Build the bundle with `flutter assemble`, not `flutter build windows`:

```bash
flutter assemble --output=build -dTargetPlatform=windows-x64 -dBuildMode=release release_bundle_windows-x64_assets
```

For debug (JIT) use `-dBuildMode=debug debug_bundle_windows-x64_assets`.

Then put the engine files next to it:

```
my_overlay/Release/
├── flutter_assets/       from assemble (debug: contains kernel_blob.bin)
├── icudtl.dat            from assemble
├── windows/app.so        from assemble (release only)
├── flutter_engine.dll    see below
├── libEGL.dll            ANGLE, only for the OpenGL renderer
└── libGLESv2.dll         ANGLE, only for the OpenGL renderer
```

`flutter_engine.dll`:

- **Debug**: from Google's prebuilt zip, with `{engine_version}` taken from
  `<flutter_sdk>/bin/internal/engine.version`:
  `https://storage.googleapis.com/flutter_infra_release/flutter/{engine_version}/windows-x64/windows-x64-embedder.zip`
- **Release**: there is no prebuilt one. Build it from the Flutter engine source.

The ANGLE DLLs come from a Chrome install or the
[ANGLE repo](https://github.com/google/angle).

## Usage

```toml
[dependencies]
flutter_rust_windows_embedder = { git = "https://github.com/Vluurie/flutter_overlay_d3d11.git", branch = "master" }
```

Get the manager with `get_flutter_overlay_manager_handle()` and call it from
four places in your app:

1. **At startup, before installing hooks:** `preload_flutter_runtime_dlls(dir)`.
2. **Once, on the first present:**
   `om.init_instance(swap_chain, dir, "main_ui", dart_args, engine_args)`.
3. **Every frame, in your present hook:** `om.render_ui()`.
4. **In your WndProc:** `om.forward_input_to_flutter(hwnd, msg, wparam, lparam)`,
   which returns `true` when Flutter consumed the message.
   **In your ResizeBuffers hook:** `om.resize_flutter_overlays(swap_chain, x, y, width, height)`.

For a standalone window instead, call `init_flutter_window()` or
`init_flutter_window_from_dir(Some(dir))`.

Full API: `cargo doc --no-deps --open`. A complete working setup is the test
harness in
[`src/software_renderer/multiview/tests/harness.rs`](src/software_renderer/multiview/tests/harness.rs).

`software_renderer` is a legacy module name; it contains all three renderers.

## License

- This crate: MIT. See [LICENSE](./LICENSE).
- Flutter engine and C API bindings: BSD 3-Clause. See
  [LICENSE-THIRD-PARTY](./LICENSE-THIRD-PARTY).
