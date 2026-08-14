# embedder_test_app

The Flutter app used by this crate's `engine-tests` (the harness under
`src/software_renderer/multiview/tests/`). The crate's `build.rs` runs
`flutter assemble` on this project and stages the engine runtime next to the
output, then the tests drive a real engine against a real D3D11 device.

Only the source is tracked (`lib/`, `pubspec.*`, `analysis_options.yaml`). The
`build/` output and the `.dart_tool/` are generated and gitignored. The C++
runner (`windows/`) is intentionally absent: the embedder uses `flutter assemble`,
which does not need it.

## Engine artifacts (not in git)

The `engine-tests` need the Flutter engine runtime, which is **not** committed.
The current version is **3.47.0**. Place the binaries here, matching this exact
layout (the version folder name must be `flutter_engine_<version>`):

```
flutter_artifacts/test_libs/flutter-engine-artifacts/
└── flutter_engine_3.47.0/
    ├── Debug/      (JIT, and the fallback if Release is missing)
    │   ├── flutter_engine.dll
    │   ├── icudtl.dat
    │   ├── libEGL.dll
    │   └── libGLESv2.dll
    └── Release/    (AOT)
        ├── flutter_engine.dll
        ├── icudtl.dat
        ├── libEGL.dll
        └── libGLESv2.dll
```

`build.rs` copies the four files from `Release/` for an AOT (release) bundle, and
from `Debug/` for a JIT (debug) bundle (it also falls back to `Debug/` if a
`Release/` set is not present). See the repo root README, "Get the Flutter
runtime files", for where each file comes from (`flutter_engine.dll`: Google zip
for JIT, compiled yourself for AOT; ANGLE DLLs from Chrome or the ANGLE repo).

## Running the engine-tests

From the crate root:

```bash
cargo test --features engine-tests --lib -- --nocapture --test-threads=1
```

For the AOT/release engine path, set `FLUTTER_TEST_RELEASE=1` first.
