#[allow(unused_imports)]
use std::{env, path::PathBuf};

mod keyboard_map;

#[cfg(feature = "regenerate-bindings")]
mod regenerate_assets {
    use super::keyboard_map;
    use bindgen::Builder;
    use bindgen::callbacks::CargoCallbacks;
    use keyboard_map::gen_keyboard_map::generate_keyboard_map;
    use std::{env, path::PathBuf};

    pub fn run() {
        println!(
            "cargo:warning=Feature 'regenerate-bindings' is active. Regenerating all assets..."
        );

        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let bindings_out_path = manifest_dir.join("src").join("bindings");
        if !bindings_out_path.exists() {
            std::fs::create_dir_all(&bindings_out_path)
                .expect("Could not create bindings directory");
        }

        let keyboard_map_out_path = bindings_out_path.join("keyboard_layout.rs");
        if let Err(e) = generate_keyboard_map("windows", &keyboard_map_out_path) {
            eprintln!("Error generating keyboard map: {}", e);
            std::process::exit(1);
        }
        println!(
            "cargo:warning=Keyboard map has been regenerated into {}.",
            keyboard_map_out_path.display()
        );

        let artifacts_dir = manifest_dir.join("flutter_artifacts");
        let include_dir = artifacts_dir.join("include");
        let header_windows = include_dir.join("flutter_windows.h");
        let header_embedder = include_dir.join("flutter_embedder.h");

        assert!(header_windows.is_file(), "flutter_windows.h not found");
        assert!(header_embedder.is_file(), "flutter_embedder.h not found");

        let bindings_windows = Builder::default()
            .header(header_windows.to_str().unwrap())
            .clang_arg(format!("-I{}", include_dir.display()))
            .allowlist_type("FlutterDesktopEngineRef")
            .allowlist_type("FlutterDesktopPluginRegistrarRef")
            .allowlist_type("FlutterDesktopViewControllerRef")
            .allowlist_type("FlutterDesktopViewRef")
            .allowlist_type("FlutterDesktopEngineProperties")
            .allowlist_type("HWND")
            .allowlist_type("WPARAM")
            .allowlist_type("LPARAM")
            .allowlist_type("LRESULT")
            .allowlist_type("UINT")
            .allowlist_function("FlutterDesktopEngineCreate")
            .allowlist_function("FlutterDesktopEngineDestroy")
            .allowlist_function("FlutterDesktopEngineGetPluginRegistrar")
            .allowlist_function("FlutterDesktopEngineProcessExternalWindowMessage")
            .allowlist_function("FlutterDesktopViewControllerCreate")
            .allowlist_function("FlutterDesktopViewControllerGetView")
            .allowlist_function("FlutterDesktopViewControllerGetEngine")
            .allowlist_function("FlutterDesktopViewControllerHandleTopLevelWindowProc")
            .allowlist_function("FlutterDesktopViewControllerDestroy")
            .allowlist_function("FlutterDesktopViewGetHWND")
            .generate()
            .expect("Unable to generate flutter_windows bindings");

        bindings_windows
            .write_to_file(bindings_out_path.join("flutter_windows_bindings.rs"))
            .expect("Couldn't write flutter_windows bindings");

        let bindings_embedder = Builder::default()
            .header(header_embedder.to_str().unwrap())
            .clang_arg(format!("-I{}", include_dir.display()))
            .parse_callbacks(Box::new(CargoCallbacks::new()))
            .allowlist_type("FlutterEngine")
            .allowlist_type("FlutterProjectArgs")
            .allowlist_type("FlutterRendererConfig")
            .allowlist_type("FlutterWindowMetricsEvent")
            .allowlist_type("FlutterCustomTaskRunners")
            .allowlist_type("FlutterPointerEvent")
            .allowlist_type("FlutterPointerPhase")
            .allowlist_type("FlutterPointerDeviceKind")
            .allowlist_type("FlutterPointerSignalKind")
            .allowlist_type("FlutterKeyEvent")
            .allowlist_type("FlutterKeyEventDeviceType")
            .allowlist_type("FlutterKeyEventType")
            .allowlist_type("FlutterPlatformMessage")
            .allowlist_type("FlutterTaskRunnerDescription")
            .allowlist_type("FlutterEngineAOTDataSource")
            .allowlist_type("FlutterEngineAOTDataSourceType")
            .allowlist_type("FlutterEngineAOTDataSource__bindgen_ty_1")
            .allowlist_type("FlutterSoftwareRendererConfig")
            // --- Multi-view (AddView/RemoveView) ---
            .allowlist_type("FlutterViewId")
            .allowlist_type("FlutterAddViewInfo")
            .allowlist_type("FlutterAddViewResult")
            .allowlist_type("FlutterAddViewCallback")
            .allowlist_type("FlutterRemoveViewInfo")
            .allowlist_type("FlutterRemoveViewResult")
            .allowlist_type("FlutterRemoveViewCallback")
            // --- Compositor / backing-store / layers (multi-view present path) ---
            .allowlist_type("FlutterCompositor")
            .allowlist_type("FlutterBackingStore")
            .allowlist_type("FlutterBackingStoreConfig")
            .allowlist_type("FlutterBackingStoreType")
            .allowlist_type("FlutterOpenGLBackingStore")
            .allowlist_type("FlutterSoftwareBackingStore")
            .allowlist_type("FlutterSoftwareBackingStore2")
            .allowlist_type("FlutterSoftwarePixelFormat")
            .allowlist_type("FlutterLayer")
            .allowlist_type("FlutterLayerContentType")
            .allowlist_type("FlutterBackingStorePresentInfo")
            .allowlist_type("FlutterPresentViewInfo")
            .allowlist_type("FlutterPresentInfo")
            .allowlist_type("FlutterRendererType")
            .allowlist_type("FlutterTask")
            .allowlist_type("FlutterEngineResult")
            .allowlist_type("FlutterPlatformMessageResponseHandle")
            .allowlist_type("FlutterDesktopBinaryReply")
            .allowlist_type("FlutterSoftwareSurfacePresentCallback")
            .allowlist_function("FlutterEngineRun")
            .allowlist_function("FlutterEngineShutdown")
            .allowlist_function("FlutterEngineInitialize")
            .allowlist_function("FlutterEngineRunInitialized")
            .allowlist_function("FlutterEngineDeinitialize")
            .allowlist_function("FlutterEngineUpdateSemanticsEnabled")
            .allowlist_function("FlutterEngineUpdateLocales")
            .allowlist_type("FlutterLocale")
            .allowlist_function("FlutterEngineSendWindowMetricsEvent")
            .allowlist_function("FlutterEngineAddView")
            .allowlist_function("FlutterEngineRemoveView")
            .allowlist_function("FlutterEngineSendViewFocusEvent")
            .allowlist_type("FlutterViewFocusEvent")
            .allowlist_type("FlutterViewFocusState")
            .allowlist_type("FlutterViewFocusDirection")
            .allowlist_function("FlutterEngineGetCurrentTime")
            .allowlist_function("FlutterEngineSendPointerEvent")
            .allowlist_function("FlutterEngineSendKeyEvent")
            .allowlist_function("FlutterEngineSendPlatformMessage")
            .allowlist_function("FlutterEngineSendPlatformMessageResponse")
            .allowlist_function("FlutterPlatformMessageCreateResponseHandle")
            .allowlist_function("FlutterEngineCreateAOTData")
            .allowlist_function("FlutterEngineRunTask")
            .allowlist_function("FlutterEngineScheduleFrame")
            .allowlist_function("FlutterEngineOnVsync")
            .allowlist_function("FlutterEngineScheduleFrame")
            .allowlist_type("FlutterEngineDartObject")
            .allowlist_function("FlutterEnginePostDartObject")
            .allowlist_type("FlutterEngineDartPort")
            .allowlist_type("FlutterEngineDartBuffer")
            .allowlist_type("FlutterEngineDartObjectType")
            .allowlist_type("FlutterOpenGLRendererConfig")
            .allowlist_type("FlutterOpenGLTexture")
            .allowlist_type("FlutterOpenGLFramebuffer")
            .allowlist_type("FlutterOpenGLSurface")
            .allowlist_type("FlutterOpenGLTargetType")
            .allowlist_type("BoolCallback")
            .allowlist_type("UIntCallback")
            .allowlist_type("ProcResolver")
            .generate()
            .expect("Unable to generate flutter_embedder bindings");

        bindings_embedder
            .write_to_file(bindings_out_path.join("flutter_embedder_bindings.rs"))
            .expect("Couldn't write flutter_embedder bindings");

        println!("cargo:warning=Bindings have been regenerated in src/bindings/.");

        println!("cargo:rerun-if-changed=build.rs");
        println!("cargo:rerun-if-changed=keyboard_map/mod.rs");
        println!("cargo:rerun-if-changed=keyboard_map/gen_keyboard_map.rs");
    }
}

#[cfg(feature = "engine-tests")]
mod engine_test_assets {
    use reqwest::blocking::get as http_get;
    use std::path::{Path, PathBuf};
    use std::{env, fs, io, process};
    use zip::ZipArchive;

    const FLUTTER_VERSION: &str = "3.35.7";

    pub fn run() {
        // Release/AOT when FLUTTER_TEST_RELEASE is set (any non-empty value),
        // otherwise debug/JIT. Release produces `build/windows/app.so` (the AOT
        // ELF the embedder loads); debug produces `build/flutter_assets/
        // kernel_blob.bin`. Switching the env var re-runs this build script.
        let release = env::var("FLUTTER_TEST_RELEASE")
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let test_app = manifest_dir
            .join("flutter_artifacts")
            .join("test_libs")
            .join("test_app");
        let build_dir = test_app.join("build");
        let bundle = build_dir.join("flutter_assets");
        let kernel = bundle.join("kernel_blob.bin");
        let aot_so = build_dir.join("windows").join("app.so");

        println!("cargo:rerun-if-changed={}", test_app.join("lib").display());
        println!("cargo:rerun-if-changed={}", kernel.display());
        println!("cargo:rerun-if-changed={}", aot_so.display());
        println!("cargo:rerun-if-env-changed=FLUTTER_TEST_RELEASE");

        // Already-built artifact for the requested mode? Skip.
        let built = if release { aot_so.is_file() } else { kernel.is_file() };
        if built {
            return;
        }

        let flutter_exe = match setup_flutter_sdk(&manifest_dir) {
            Ok(p) => p,
            Err(e) => {
                println!("cargo:warning=engine-tests: SDK setup failed: {e}");
                return;
            }
        };

        let sdk_root = flutter_exe.parent().unwrap().parent().unwrap();
        let new_path = format!(
            "{};{};{}",
            sdk_root.join("bin").display(),
            sdk_root.join("bin/cache/dart-sdk/bin").display(),
            env::var("PATH").unwrap_or_default()
        );
        unsafe { env::set_var("PATH", &new_path) };

        let _ = run_cmd(process::Command::new(&flutter_exe).arg("pub").arg("get").current_dir(&test_app));

        let (build_mode, target) = if release {
            ("release", "release_bundle_windows-x64_assets")
        } else {
            ("debug", "debug_bundle_windows-x64_assets")
        };
        let assembled = run_cmd(
            process::Command::new(&flutter_exe)
                .arg("assemble")
                .arg("--output=build")
                .arg("-dTargetPlatform=windows-x64")
                .arg(format!("-dBuildMode={build_mode}"))
                .arg(target)
                .current_dir(&test_app),
        );

        let produced = if release { aot_so.is_file() } else { kernel.is_file() };
        if !assembled || !produced {
            println!(
                "cargo:warning=engine-tests: bundle build did not produce {}",
                if release { "windows/app.so" } else { "kernel_blob.bin" }
            );
            return;
        }

        if let Err(e) = stage_engine_runtime(&manifest_dir, &build_dir, release) {
            println!("cargo:warning=engine-tests: staging engine runtime failed: {e}");
        }
    }

    fn stage_engine_runtime(
        manifest_dir: &Path,
        test_root: &Path,
        release: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let artifacts = manifest_dir
            .join("flutter_artifacts")
            .join("test_libs")
            .join("flutter-engine-artifacts")
            .join(format!("flutter_engine_{FLUTTER_VERSION}"));
        // Prefer the Release engine DLLs for an AOT bundle, but fall back to
        // Debug if a Release set was not downloaded.
        let preferred = artifacts.join(if release { "Release" } else { "Debug" });
        let src_dir = if preferred.join("flutter_engine.dll").is_file() {
            preferred
        } else {
            artifacts.join("Debug")
        };
        for f in [
            "flutter_engine.dll",
            "icudtl.dat",
            "libEGL.dll",
            "libGLESv2.dll",
        ] {
            let src = src_dir.join(f);
            if src.is_file() {
                fs::copy(&src, test_root.join(f))?;
            }
        }
        Ok(())
    }

    fn run_cmd(cmd: &mut process::Command) -> bool {
        match cmd.output() {
            Ok(o) if o.status.success() => true,
            Ok(o) => {
                println!(
                    "cargo:warning=engine-tests cmd failed: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
                false
            }
            Err(e) => {
                println!("cargo:warning=engine-tests cmd error: {e}");
                false
            }
        }
    }

    fn setup_flutter_sdk(manifest_dir: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let exe_name = if cfg!(target_os = "windows") {
            "flutter.bat"
        } else {
            "flutter"
        };
        let sdk_parent = manifest_dir
            .join("target")
            .join("flutter_artifacts_cache")
            .join("flutter_sdks");
        let sdk_dir = sdk_parent.join(format!("flutter_{FLUTTER_VERSION}"));
        let exe = sdk_dir.join("bin").join(exe_name);
        if exe.exists() {
            return Ok(exe);
        }

        fs::create_dir_all(&sdk_parent)?;
        let url = format!(
            "https://storage.googleapis.com/flutter_infra_release/releases/stable/windows/flutter_windows_{FLUTTER_VERSION}-stable.zip"
        );
        let zip_path = sdk_parent.join(format!("flutter_v{FLUTTER_VERSION}.zip"));
        let mut response = http_get(&url)?;
        if !response.status().is_success() {
            return Err(format!("download failed: HTTP {}", response.status()).into());
        }
        let mut dest = fs::File::create(&zip_path)?;
        io::copy(&mut response, &mut dest)?;

        let file = fs::File::open(&zip_path)?;
        let mut archive = ZipArchive::new(file)?;
        archive.extract(&sdk_parent)?;
        let extracted = sdk_parent.join("flutter");
        if extracted.exists() {
            fs::rename(&extracted, &sdk_dir)?;
        }
        let _ = fs::remove_file(&zip_path);

        if !exe.exists() {
            return Err("flutter executable missing after extraction".into());
        }
        Ok(exe)
    }
}

fn main() {
    #[cfg(feature = "regenerate-bindings")]
    regenerate_assets::run();
    #[cfg(feature = "engine-tests")]
    engine_test_assets::run();
}
