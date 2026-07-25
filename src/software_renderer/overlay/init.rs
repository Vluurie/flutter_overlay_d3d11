use crate::path_utils::load_flutter_build_paths;
use crate::software_renderer::api::{OverlayCreateParams, RendererType};
use crate::software_renderer::d3d11_compositor::effects::EffectConfig;
use crate::software_renderer::d3d11_compositor::post_processing_renderer::PostProcessRenderer;
use crate::software_renderer::d3d11_compositor::primitive_3d_renderer::Primitive3DRenderer;
use crate::software_renderer::d3d11_compositor::text_3d_renderer::Text3DRenderer;
use crate::software_renderer::dynamic_flutter_engine_dll_loader::FlutterEngineDll;
use windows::core::Interface;

use crate::software_renderer::gl_renderer::angle_interop::{
    AngleInteropState, SendableAngleState, build_opengl_renderer_config,
};
use crate::software_renderer::overlay::d3d::{
    create_compositing_texture, create_srv, create_texture,
};
use crate::software_renderer::overlay::engine::{
    on_root_isolate_created, run_engine, send_system_locale_to_engine, update_flutter_window_metrics,
};
use crate::software_renderer::overlay::overlay_impl::{
    FLUTTER_LOG_TAG, SendHwnd, SendableFlutterEngine, SendableHandle,
};
use crate::software_renderer::overlay::platform_message_callback::simple_platform_message_callback;
use crate::software_renderer::overlay::textinput::{
    ViewKeyboardState, register_view_keyboard_state,
};
use crate::software_renderer::overlay::project_args::{
    build_project_args_and_strings, flutter_log_callback, maybe_load_aot_path_to_cstring,
};
use crate::software_renderer::multiview::ViewRegistry;
use crate::software_renderer::multiview::compositor::{
    build_compositor, view_focus_change_request_callback,
};
use crate::software_renderer::overlay::renderer::build_software_renderer_config;

use crate::bindings::embedder::{
    self, FlutterCustomTaskRunners, FlutterEngineAOTDataSource,
    FlutterEngineAOTDataSourceType_kFlutterEngineAOTDataSourceTypeElfPath,
    FlutterEngineResult_kSuccess, FlutterProjectArgs, FlutterTaskRunnerDescription,
};
use crate::software_renderer::overlay::semantics_handler::semantics_update_callback;
use crate::software_renderer::ticker::spawn::start_task_runner;
use crate::software_renderer::ticker::task_runner_window::Waker;
use crate::software_renderer::ticker::task_scheduler::{
    SendableFlutterCustomTaskRunners, SendableFlutterTaskRunnerDescription, TaskQueueState,
    TaskRunnerContext, destroy_task_runner_context_callback, post_task_callback,
    runs_task_on_current_thread_callback,
};

use log::error;
use std::collections::{HashMap, VecDeque};
use std::ffi::{CString, c_char};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};
use std::{ffi::c_void, path::PathBuf, ptr};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_QUERY_DESC, D3D11_QUERY_EVENT, ID3D11Device, ID3D11Query, ID3D11ShaderResourceView,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{DXGI_SWAP_CHAIN_DESC, IDXGIKeyedMutex, IDXGISwapChain};

use super::overlay_impl::FlutterOverlay;

/// GPU + renderer resources produced when selecting/initializing a renderer
/// (OpenGL/ANGLE or the software fallback) for a new overlay. Replaces a large
/// anonymous tuple so the type stays readable.
struct RendererInitResources {
    rdr_cfg: embedder::FlutterRendererConfig,
    texture: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    gl_internal_linear_texture: Option<ID3D11Texture2D>,
    pixel_buffer: Option<Vec<u8>>,
    angle_state: Option<SendableAngleState>,
    d3d11_shared_handle: Option<SendableHandle>,
    angle_shared_texture: Option<ID3D11Texture2D>,
    angle_query: Option<ID3D11Query>,
    angle_keyed_mutex: Option<IDXGIKeyedMutex>,
    game_keyed_mutex: Option<IDXGIKeyedMutex>,
    renderer_type: RendererType,
}

const FLUTTER_ENGINE_VERSION: usize = 1;
///  global flag tracks if a hardware-accelerated (OpenGL) context has already been created. Currently support only 1 overlay with it.
/// other fallback to software renderer
static OPENGL_CONTEXT_CREATED: AtomicBool = AtomicBool::new(false);

pub(crate) fn init_overlay(
    params: OverlayCreateParams,
    device: &ID3D11Device,
    swap_chain: &IDXGISwapChain,
) -> Option<Box<FlutterOverlay>> {
    let OverlayCreateParams {
        name,
        x,
        y,
        width,
        height,
        flutter_data_dir,
        dart_entrypoint_args,
        engine_args,
    } = params;
    let data_dir: Option<PathBuf> = Some(flutter_data_dir);
    let dart_args_opt: Option<&[String]> = dart_entrypoint_args.as_deref();
    let engine_args_opt: Option<&[String]> = engine_args.as_deref();

    unsafe {
        let engine_dll_load_dir = data_dir.as_deref();
        let engine_dll_arc = match FlutterEngineDll::get_for(engine_dll_load_dir) {
            Ok(dll) => dll,
            Err(e) => {
                error!(
                    "Failed to load flutter_engine.dll from `{engine_dll_load_dir:?}`: {e:?}"
                );
                return None;
            }
        };

        if width == 0 || height == 0 {
            error!(
                "Width and height must be non-zero, got {width}x{height}"
            );
            return None;
        }

        let (assets, icu, aot_opt) = load_flutter_build_paths(data_dir.clone());
        let initial_is_debug = aot_opt.is_none();

        let (assets_c_temp, icu_c_temp, engine_argv_cs_temp, mut dart_argv_cs_temp) =
            build_project_args_and_strings(
                &assets.to_string_lossy(),
                &icu.to_string_lossy(),
                dart_args_opt,
                engine_args_opt,
            );

        let aot_c_temp = maybe_load_aot_path_to_cstring(aot_opt.as_deref());

        let swap_chain_desc: DXGI_SWAP_CHAIN_DESC = match swap_chain.GetDesc() {
            Ok(desc) => desc,
            Err(e) => {
                error!("Failed to get swap chain description: {e}");
                return None;
            }
        };
        let hwnd = swap_chain_desc.OutputWindow;
        let game_device: &ID3D11Device = device;

        let RendererInitResources {
            rdr_cfg,
            texture: texture_for_struct,
            srv: srv_for_struct,
            gl_internal_linear_texture: gl_internal_linear_texture_for_struct,
            pixel_buffer: pixel_buffer_for_struct,
            angle_state: angle_state_for_struct,
            d3d11_shared_handle: d3d11_shared_handle_for_struct,
            angle_shared_texture: angle_shared_texture_for_struct,
            angle_query: angle_query_for_struct,
            angle_keyed_mutex: angle_keyed_mutex_for_struct,
            game_keyed_mutex: game_keyed_mutex_for_struct,
            renderer_type: final_renderer_type,
        } = 'opengl_attempt: {
            if OPENGL_CONTEXT_CREATED
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                let opengl_init_result =
                    AngleInteropState::new(data_dir.as_deref()).and_then(|mut state| {
                        state
                            .recreate_resources(width, height)
                            .map(|(texture, handle)| (state, texture, handle))
                    });

                match opengl_init_result {
                    Ok((angle_state, angle_texture_on_angle_device, shared_handle)) => {
                        let mut opt: Option<ID3D11Texture2D> = None;
                        let angle_texture_on_game_device: ID3D11Texture2D = match game_device
                            .OpenSharedResource(shared_handle, &mut opt)
                            .ok()
                            .and(opt)
                        {
                            Some(tex) => tex,
                            None => {
                                error!(
                                    "[InitOverlay] OpenSharedResource failed for '{name}'. Falling back to software renderer.",
                                );
                                drop(angle_state);
                                OPENGL_CONTEXT_CREATED.store(false, Ordering::SeqCst);
                                break 'opengl_attempt build_software_renderer_config_tuple(
                                    game_device,
                                    width,
                                    height,
                                );
                            }
                        };

                        let angle_km: Option<IDXGIKeyedMutex> =
                            angle_texture_on_angle_device.cast().ok();
                        let game_km: Option<IDXGIKeyedMutex> =
                            angle_texture_on_game_device.cast().ok();

                        if angle_km.is_none() || game_km.is_none() {
                            error!(
                                "[InitOverlay] Failed to get IDXGIKeyedMutex - cross-device sync unavailable!"
                            );
                        }

                        let local_texture_on_game_device =
                            create_compositing_texture(game_device, width, height);
                        let texture = local_texture_on_game_device;
                        let srv = create_srv(game_device, &texture);

                        let angle_query: Option<ID3D11Query> = {
                            let query_desc = D3D11_QUERY_DESC {
                                Query: D3D11_QUERY_EVENT,
                                MiscFlags: 0,
                            };
                            let mut query_opt: Option<ID3D11Query> = None;
                            if angle_state
                                .angle_d3d11_device
                                .CreateQuery(&query_desc, Some(&mut query_opt))
                                .is_ok()
                            {
                                query_opt
                            } else {
                                error!(
                                    "[InitOverlay] Failed to create D3D11 event query - flickering may occur!"
                                );
                                None
                            }
                        };

                        let angle_state = Some(SendableAngleState(angle_state));
                        let rdr_cfg = build_opengl_renderer_config();

                        RendererInitResources {
                            rdr_cfg,
                            texture,
                            srv,
                            gl_internal_linear_texture: Some(angle_texture_on_angle_device),
                            pixel_buffer: None,
                            angle_state,
                            d3d11_shared_handle: Some(SendableHandle(shared_handle)),
                            angle_shared_texture: Some(angle_texture_on_game_device),
                            angle_query,
                            angle_keyed_mutex: angle_km,
                            game_keyed_mutex: game_km,
                            renderer_type: RendererType::OpenGL,
                        }
                    }
                    Err(e) => {
                        // If even the first attempt fails, reset the flag and fall back.
                        error!(
                            "OpenGL initialization failed for overlay: {e}. Falling back to software."
                        );
                        OPENGL_CONTEXT_CREATED.store(false, Ordering::SeqCst);
                        build_software_renderer_config_tuple(game_device, width, height)
                    }
                }
            } else {
                build_software_renderer_config_tuple(game_device, width, height)
            }
        };
        let post_processor = PostProcessRenderer::new(device);
        let primitive_renderer = Primitive3DRenderer::new(device);
        let text_renderer = Text3DRenderer::new(device);

        let renderer_arg = match final_renderer_type {
            RendererType::OpenGL => "--renderer=opengl",
            RendererType::Software => "--renderer=software",
        };
        if let Ok(arg) = CString::new(renderer_arg) {
            dart_argv_cs_temp.push(arg);
        }

        let compositor_active = matches!(final_renderer_type, RendererType::OpenGL);

        let engine_atomic_ptr_instance = Arc::new(AtomicPtr::new(ptr::null_mut()));
        let task_queue_arc = Arc::new(TaskQueueState::new(Arc::new(Waker::new())));

        let platform_context_owned_by_overlay = Box::new(TaskRunnerContext {
            task_runner_thread_id: None,
            task_queue: task_queue_arc.clone(),
        });

        let mut overlay_box = Box::new(FlutterOverlay {
            name,
            engine: SendableFlutterEngine(ptr::null_mut()),
            engine_atomic_ptr: engine_atomic_ptr_instance.clone(),
            pixel_buffer: pixel_buffer_for_struct,
            software_frame_dirty: AtomicBool::new(false),
            software_first_frame_rendered: AtomicBool::new(false),
            width,
            height,
            visible: true,
            keep_alive: false,
            ui_hidden: false,
            effect_config: EffectConfig::default(),
            effect_frames_remaining: 0,
            effect_total_frames: 0,
            x,
            y,
            texture: texture_for_struct,
            srv: srv_for_struct,
            gl_internal_linear_texture: gl_internal_linear_texture_for_struct,
            post_processor,
            primitive_renderer,
            text_renderer,
            desired_cursor: Arc::new(Mutex::new(None)),
            task_queue_state: task_queue_arc,
            task_runner_thread: None,
            message_handlers: Arc::new(Mutex::new(HashMap::new())),
            response_buffer: Arc::new(Mutex::new(Vec::with_capacity(1024))), // Start with 1KB capacity
            _assets_c: assets_c_temp,
            _icu_c: icu_c_temp,
            _engine_argv_cs: engine_argv_cs_temp,
            _dart_argv_cs: dart_argv_cs_temp,
            _aot_c: aot_c_temp,
            _platform_runner_context: Some(platform_context_owned_by_overlay),
            _platform_runner_description: None,
            _custom_task_runners_struct: None,
            _compositor: None,
            engine_dll: engine_dll_arc.clone(),
            view0_keyboard: Arc::new(ViewKeyboardState::new()),
            active_text_input: Arc::new(Mutex::new(None)),
            pending_platform_messages: Arc::new(Mutex::new(VecDeque::new())),
            pending_key_events: Arc::new(Mutex::new(VecDeque::new())),
            pending_view_focus: Arc::new(Mutex::new(VecDeque::new())),
            mouse_buttons_state: AtomicI32::new(0),
            is_mouse_added: AtomicBool::new(false),
            semantics_tree_data: Arc::new(Mutex::new(HashMap::new())),
            is_interactive_widget_hovered: AtomicBool::new(false),
            windows_handler: SendHwnd(hwnd),
            is_debug_build: initial_is_debug,
            angle_shared_texture: angle_shared_texture_for_struct,
            angle_shared_texture_back: None,
            angle_keyed_mutex: angle_keyed_mutex_for_struct,
            game_keyed_mutex: game_keyed_mutex_for_struct,
            dart_send_port: Arc::new(AtomicI64::new(0)),
            renderer_type: final_renderer_type,
            angle_state: angle_state_for_struct,
            d3d11_shared_handle: d3d11_shared_handle_for_struct,
            angle_frame_complete_query: angle_query_for_struct,
            angle_frame_presented: std::sync::atomic::AtomicU64::new(0),
            angle_frame_copied: std::sync::atomic::AtomicU64::new(0),
            damage_rects: std::sync::Mutex::new(Vec::new()),
            frame_damage_rects: std::sync::Mutex::new(Vec::new()),
            full_repaint_needed: std::sync::atomic::AtomicBool::new(true),
            view_registry: Arc::new(ViewRegistry::new()),
            view0_gl: None,
            compositor_active,
        });

        register_view_keyboard_state(0, overlay_box.view0_keyboard.clone());

        let user_data_for_engine: *mut c_void = &mut *overlay_box as *mut _ as *mut c_void;

        start_task_runner(&mut overlay_box);

        let platform_description = FlutterTaskRunnerDescription {
            struct_size: std::mem::size_of::<FlutterTaskRunnerDescription>(),
            user_data: match overlay_box._platform_runner_context.as_ref() {
                Some(ctx) => ctx.as_ref() as *const _ as *mut c_void,
                None => {
                    error!("[InitOverlay] Platform runner context missing");
                    return None;
                }
            },
            runs_task_on_current_thread_callback: Some(runs_task_on_current_thread_callback),
            post_task_callback: Some(post_task_callback),
            identifier: 1,
            destruction_callback: Some(destroy_task_runner_context_callback),
        };

        let platform_description_box =
            Box::new(SendableFlutterTaskRunnerDescription(platform_description));

        let custom_task_runners = FlutterCustomTaskRunners {
            struct_size: std::mem::size_of::<FlutterCustomTaskRunners>(),
            platform_task_runner: &platform_description_box.0,
            render_task_runner: &platform_description_box.0,
            thread_priority_setter: None,
            ui_task_runner: &platform_description_box.0, // Merged thread mode: UI runs on platform thread
        };

        let custom_task_runners_box =
            Box::new(SendableFlutterCustomTaskRunners(custom_task_runners));

        overlay_box._platform_runner_description = Some(platform_description_box);
        overlay_box._custom_task_runners_struct = Some(custom_task_runners_box);

        let engine_argv_ptrs: Vec<*const c_char> = overlay_box
            ._engine_argv_cs
            .iter()
            .map(|c| c.as_ptr())
            .collect();
        let dart_argv_ptrs: Vec<*const c_char> = overlay_box
            ._dart_argv_cs
            .iter()
            .map(|c| c.as_ptr())
            .collect();

        let mut proj_args = FlutterProjectArgs {
            struct_size: std::mem::size_of::<FlutterProjectArgs>(),
            assets_path: overlay_box._assets_c.as_ptr(),
            main_path__unused__: ptr::null(),
            packages_path__unused__: ptr::null(),
            icu_data_path: overlay_box._icu_c.as_ptr(),
            command_line_argc: engine_argv_ptrs.len() as i32,
            command_line_argv: if engine_argv_ptrs.is_empty() {
                ptr::null()
            } else {
                engine_argv_ptrs.as_ptr()
            },
            platform_message_callback: Some(simple_platform_message_callback),
            vm_snapshot_data: ptr::null(),
            vm_snapshot_data_size: 0,
            vm_snapshot_instructions: ptr::null(),
            vm_snapshot_instructions_size: 0,
            isolate_snapshot_data: ptr::null(),
            isolate_snapshot_data_size: 0,
            isolate_snapshot_instructions: ptr::null(),
            isolate_snapshot_instructions_size: 0,
            root_isolate_create_callback: Some(on_root_isolate_created),
            update_semantics_node_callback: None,
            update_semantics_custom_action_callback: None,
            persistent_cache_path: ptr::null(),
            is_persistent_cache_read_only: false,
            vsync_callback: None,
            custom_dart_entrypoint: ptr::null(),
            custom_task_runners: overlay_box
                ._custom_task_runners_struct
                .as_ref()
                .map_or(ptr::null(), |b| &b.0),
            shutdown_dart_vm_when_done: true,
            compositor: ptr::null(),
            dart_old_gen_heap_size: -1,
            aot_data: ptr::null_mut(),
            compute_platform_resolved_locale_callback: None,
            dart_entrypoint_argc: dart_argv_ptrs.len() as i32,
            dart_entrypoint_argv: if dart_argv_ptrs.is_empty() {
                ptr::null()
            } else {
                dart_argv_ptrs.as_ptr()
            },
            log_message_callback: Some(flutter_log_callback),
            log_tag: FLUTTER_LOG_TAG.as_ptr(),
            on_pre_engine_restart_callback: None,
            update_semantics_callback: None,
            update_semantics_callback2: Some(semantics_update_callback),
            channel_update_callback: None,
            view_focus_change_request_callback: if compositor_active {
                Some(view_focus_change_request_callback)
            } else {
                None
            },
            engine_id: 0,
        };

        if let Some(aot_c_ref) = &overlay_box._aot_c {
            let source = FlutterEngineAOTDataSource {
                type_: FlutterEngineAOTDataSourceType_kFlutterEngineAOTDataSourceTypeElfPath,
                __bindgen_anon_1: embedder::FlutterEngineAOTDataSource__bindgen_ty_1 {
                    elf_path: aot_c_ref.as_ptr(),
                },
            };
            let res = (overlay_box.engine_dll.FlutterEngineCreateAOTData)(
                &source,
                &mut proj_args.aot_data,
            );
            if res != FlutterEngineResult_kSuccess {
                error!(
                    "[InitOverlay] FlutterEngineCreateAOTData failed with code: {:?}, for AOT file: {}",
                    res,
                    aot_c_ref.to_string_lossy()
                );
                proj_args.aot_data = ptr::null_mut();
            }
        }

        overlay_box.is_debug_build = proj_args.aot_data.is_null();

        // Install the multi-view compositor for the OpenGL renderer. Its
        // callbacks reach the overlay (and its view registry) through this
        // pointer, which is stable because `overlay_box` is heap-allocated and
        // not moved after this point. The compositor is held in `_compositor`
        // for the engine's lifetime since `proj_args.compositor` borrows it.
        if overlay_box.compositor_active {
            use crate::software_renderer::overlay::overlay_impl::SendableFlutterCompositor;

            let host_ptr: *mut FlutterOverlay = &mut *overlay_box;
            let compositor = build_compositor(host_ptr);
            let compositor_box = Box::new(SendableFlutterCompositor(compositor));
            proj_args.compositor = &compositor_box.0;
            overlay_box._compositor = Some(compositor_box);
        }

        let raw_ptr_to_overlay_for_run_engine: *mut FlutterOverlay = &mut *overlay_box;
        let engine_run_result = run_engine(
            FLUTTER_ENGINE_VERSION,
            &rdr_cfg,
            &proj_args,
            user_data_for_engine,
            raw_ptr_to_overlay_for_run_engine,
            overlay_box.engine_dll.clone(),
        );

        let engine_handle = match engine_run_result {
            Ok(handle) => handle,
            Err(e) => {
                error!(
                    "[InitOverlay] Engine initialization failed during run_engine: {e}"
                );
                engine_atomic_ptr_instance.store(ptr::null_mut(), Ordering::SeqCst);
                return None;
            }
        };

        (engine_dll_arc.FlutterEngineUpdateSemanticsEnabled)(engine_handle, true);

        send_system_locale_to_engine(engine_handle, &engine_dll_arc);

        overlay_box.engine = SendableFlutterEngine(engine_handle);
        engine_atomic_ptr_instance.store(engine_handle, Ordering::SeqCst);

        update_flutter_window_metrics(engine_handle, x, y, width, height, engine_dll_arc.clone());

        Some(overlay_box)
    }
}

fn build_software_renderer_config_tuple(
    game_device: &ID3D11Device,
    width: u32,
    height: u32,
) -> RendererInitResources {
    let texture = create_texture(game_device, width, height);
    let srv = create_srv(game_device, &texture);
    let pixel_buffer = Some(vec![0; (width * height * 4) as usize]);
    let rdr_cfg = build_software_renderer_config();

    RendererInitResources {
        rdr_cfg,
        texture,
        srv,
        gl_internal_linear_texture: None,
        pixel_buffer,
        angle_state: None,
        d3d11_shared_handle: None,
        angle_shared_texture: None,
        angle_query: None, // No GPU sync query needed for software renderer
        angle_keyed_mutex: None, // No keyed mutex for software renderer
        game_keyed_mutex: None,
        renderer_type: RendererType::Software,
    }
}
