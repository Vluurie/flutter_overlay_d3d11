use crate::bindings::embedder::{
    FlutterEngineResult_kSuccess, FlutterKeyEvent,
    FlutterKeyEventDeviceType_kFlutterKeyEventDeviceTypeKeyboard, FlutterPlatformMessage,
    FlutterViewFocusDirection_kForward, FlutterViewFocusDirection_kUndefined, FlutterViewFocusEvent,
    FlutterViewFocusState_kFocused, FlutterViewFocusState_kUnfocused,
};

use crate::software_renderer::overlay::engine::send_system_locale_to_engine;
use crate::software_renderer::overlay::overlay_impl::FlutterOverlay;
use crate::software_renderer::ticker::task_runner_window::{Timer, run_message_loop};

use log::error;
use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::{thread, time::Duration};

extern "C" fn key_event_callback(_handled: bool, user_data: *mut c_void) {
    unsafe {
        if user_data.is_null() {
            return;
        }

        drop(Box::from_raw(user_data as *mut u64));
    }
}

pub fn start_task_runner(overlay: &mut FlutterOverlay) {
    if overlay.task_runner_thread.is_some() {
        return;
    }

    let engine_dll_for_thread = overlay.engine_dll.clone();
    let task_queue_for_thread = overlay.task_queue_state.clone();
    let name_for_thread = overlay.name.clone();
    let engine_atomic_ptr = overlay.engine_atomic_ptr.clone();
    let pending_messages_for_thread = overlay.pending_platform_messages.clone();
    let pending_keys_for_thread = overlay.pending_key_events.clone();
    let pending_view_focus_for_thread = overlay.pending_view_focus.clone();
    let pending_locale_for_thread = overlay.pending_locale_update.clone();

    let waker_for_thread = overlay.task_queue_state.waker.clone();
    let timer = Timer::new();
    timer.run(overlay.task_queue_state.waker.clone());
    let timer_for_runner = timer.clone();
    overlay.task_queue_state.set_timer(timer);

    let handle = thread::Builder::new()
        .name(format!("task_runner_{name_for_thread}"))
        .spawn(move || {
            let process = move || {
                let engine = engine_atomic_ptr.load(Ordering::SeqCst);
                if engine.is_null() {
                    return;
                }

                let expired = {
                    let mut queue_guard = task_queue_for_thread.queue.lock().unwrap();
                    let now = unsafe { (engine_dll_for_thread.FlutterEngineGetCurrentTime)() };
                    let mut due = Vec::new();
                    while let Some(task) = queue_guard.peek() {
                        if task.target_time <= now {
                            due.push(queue_guard.pop().unwrap());
                        } else {
                            let delay = task.target_time - now;
                            timer_for_runner.schedule_in(Duration::from_nanos(delay));
                            break;
                        }
                    }
                    due
                };
                for scheduled_task in expired {
                    let result = unsafe {
                        (engine_dll_for_thread.FlutterEngineRunTask)(engine, &scheduled_task.task.0)
                    };
                    if result != FlutterEngineResult_kSuccess {
                        error!("[TaskRunner] FlutterEngineRunTask failed: {result:?}");
                    }
                }

                if pending_locale_for_thread.swap(false, Ordering::AcqRel) {
                    send_system_locale_to_engine(engine, &engine_dll_for_thread);
                }

                if let Ok(mut pending_focus) = pending_view_focus_for_thread.lock() {
                    while let Some((view_id, focused)) = pending_focus.pop_front() {
                        let event = FlutterViewFocusEvent {
                            struct_size: std::mem::size_of::<FlutterViewFocusEvent>(),
                            view_id,
                            state: if focused {
                                FlutterViewFocusState_kFocused
                            } else {
                                FlutterViewFocusState_kUnfocused
                            },
                            direction: if focused {
                                FlutterViewFocusDirection_kForward
                            } else {
                                FlutterViewFocusDirection_kUndefined
                            },
                        };
                        let _ = unsafe {
                            (engine_dll_for_thread.FlutterEngineSendViewFocusEvent)(
                                engine,
                                &event as *const _,
                            )
                        };
                    }
                }

                if let Ok(mut pending_keys) = pending_keys_for_thread.lock() {
                    while let Some(key_event) = pending_keys.pop_front() {
                        if key_event.physical == 0 && key_event.logical == 0 {
                            continue;
                        }

                        let characters_cstring = CString::new(key_event.characters.clone())
                            .unwrap_or_else(|_| CString::new("").unwrap());

                        let current_time =
                            unsafe { (engine_dll_for_thread.FlutterEngineGetCurrentTime)() };

                        let event_data = FlutterKeyEvent {
                            struct_size: std::mem::size_of::<FlutterKeyEvent>(),
                            timestamp: current_time as f64 / 1000.0,
                            type_: key_event.event_type,
                            physical: key_event.physical,
                            logical: key_event.logical,
                            character: characters_cstring.as_ptr(),
                            synthesized: key_event.synthesized,
                            device_type:
                                FlutterKeyEventDeviceType_kFlutterKeyEventDeviceTypeKeyboard,
                        };

                        let physical_key_box = Box::new(key_event.physical);
                        let user_data_ptr = Box::into_raw(physical_key_box) as *mut c_void;

                        let result = unsafe {
                            (engine_dll_for_thread.FlutterEngineSendKeyEvent)(
                                engine,
                                &event_data as *const _,
                                Some(key_event_callback),
                                user_data_ptr,
                            )
                        };

                        if result != FlutterEngineResult_kSuccess {
                            error!("[TaskRunner] FlutterEngineSendKeyEvent failed: {result:?}");
                            unsafe {
                                drop(Box::from_raw(user_data_ptr as *mut u64));
                            }
                        }
                    }
                }

                if let Ok(mut pending_msgs) = pending_messages_for_thread.lock() {
                    while let Some(msg) = pending_msgs.pop_front() {
                        if let Ok(channel_cstring) = CString::new(msg.channel.as_str()) {
                            let platform_message = FlutterPlatformMessage {
                                struct_size: std::mem::size_of::<FlutterPlatformMessage>(),
                                channel: channel_cstring.as_ptr(),
                                message: msg.payload_bytes.as_ptr(),
                                message_size: msg.payload_bytes.len(),
                                response_handle: ptr::null(),
                            };
                            let _ = unsafe {
                                (engine_dll_for_thread.FlutterEngineSendPlatformMessage)(
                                    engine,
                                    &platform_message,
                                )
                            };
                        }
                    }
                }
            };

            run_message_loop(&waker_for_thread, process);
            let _ = &waker_for_thread;
        })
        .expect("Failed to spawn task runner thread");

    let thread_id = handle.thread().id();

    if let Some(context_ref_mut) = &mut overlay._platform_runner_context {
        context_ref_mut.task_runner_thread_id = Some(thread_id);
    } else {
        error!(
            "[TaskRunner] CRITICAL: _platform_runner_context is None in FlutterOverlay. Cannot set thread ID."
        );
    }

    overlay.task_runner_thread = Some(Arc::new(handle));
}
