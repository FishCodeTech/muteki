//! Android cdylib entry point loaded by the NativeActivity host.

#[cfg(target_os = "android")]
mod host;
#[cfg(target_os = "android")]
mod scroll_capture;

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub fn android_main(app: android_activity::AndroidApp) {
    use futures::{StreamExt as _, channel::mpsc};
    use gpui::{WindowBackgroundAppearance, WindowOptions};
    use std::borrow::Cow;
    use std::rc::Rc;
    use tcode_client::host::ClientHost;
    use tcode_ui::{ShellOptions, ShellSetup};

    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Debug)
            .with_filter(
                android_logger::FilterBuilder::new()
                    .parse("info,tcode_ui::store::history=debug")
                    .build(),
            )
            .with_tag("Tcode-GPUI"),
    );
    std::panic::set_hook(Box::new(|panic| {
        log::error!("GPUI Android panic: {panic}");
        log::error!("{}", std::backtrace::Backtrace::force_capture());
    }));
    gpui_android::init_platform(&app);
    gpui::Application::with_platform(gpui_android::platform())
        .with_assets(tcode_ui::assets::Assets)
        .run(move |cx| {
            // Standard Android images provide this face through /system/fonts.
            // Custom ROM hosts may supply a bitmap fallback asset if it is absent.
            if !std::path::Path::new("/system/fonts/NotoColorEmoji.ttf").exists() {
                use std::io::Read as _;
                let path = std::ffi::CString::new("fonts/NotoColorEmoji.ttf").unwrap();
                if let Some(mut asset) = app.asset_manager().open(&path) {
                    let mut font = Vec::new();
                    match asset.read_to_end(&mut font) {
                        Ok(_) => {
                            if let Err(error) = cx.text_system().add_fonts(vec![Cow::Owned(font)]) {
                                log::error!(
                                    "failed registering custom-ROM emoji fallback: {error:#}"
                                );
                            }
                        }
                        Err(error) => {
                            log::error!("failed reading custom-ROM emoji fallback: {error}")
                        }
                    }
                } else {
                    log::warn!("system emoji font absent; custom-ROM fallback asset not supplied");
                }
            }

            let (native_host, system_locale) = host::native_host(app.clone(), cx)
                .expect("failed to initialize Android host services");
            let host: Rc<dyn ClientHost> = native_host;
            tcode_ui::run_shell(
                cx,
                host.clone(),
                ShellOptions {
                    window: WindowOptions {
                        // The activity owns the geometry; the shell reads it back.
                        window_bounds: None,
                        titlebar: None,
                        window_background: WindowBackgroundAppearance::Opaque,
                        ..Default::default()
                    },
                    theme_json: Cow::Owned(tcode_ui::flattened_theme_json()),
                    activate: true,
                    system_locale,
                    // The activity is stopped in the background; the shell
                    // reconnects on return.
                    lifecycle: Some(gpui_android::platform()),
                    setup: ShellSetup {
                        initial: tcode_ui::last_host_target(host.as_ref()),
                        initial_pairing_error: None,
                        client_host: Some(host),
                        local: None,
                        local_failure: None,
                        seed_blocking: false,
                        restore_navigation: true,
                    },
                    ..Default::default()
                }
                .with_bundled_monospace(),
            );

            let (back_sender, mut back_receiver) = mpsc::unbounded();
            gpui_android::set_back_callback(move || {
                let _ = back_sender.unbounded_send(());
            });
            cx.spawn(async move |cx| {
                while back_receiver.next().await.is_some() {
                    cx.update(|cx| {
                        // The shell dismisses the keyboard, then the topmost
                        // overlay, then its navigation stack. `false` only at
                        // the root, where Android closes the app.
                        if !tcode_ui::handle_back(cx) {
                            cx.quit();
                        }
                    });
                }
            })
            .detach();

            let (capture_sender, mut capture_receiver) = mpsc::unbounded();
            gpui_android::set_scroll_capture_callback(move |request| {
                let _ = capture_sender.unbounded_send(request);
            });
            cx.spawn(async move |cx| {
                while let Some(request) = capture_receiver.next().await {
                    cx.update(|cx| scroll_capture::handle(request, cx));
                }
            })
            .detach();
        });
}

#[cfg(target_os = "android")]
mod jni_exports {
    use jni::{
        EnvUnowned,
        errors::LogErrorAndDefault,
        objects::{JByteArray, JObject, JString},
        sys::{jboolean, jint, jlong},
    };

    fn optional_string<'local>(
        env: &mut jni::Env<'local>,
        value: JObject<'local>,
    ) -> jni::errors::Result<Option<String>> {
        if value.is_null() {
            return Ok(None);
        }
        JString::cast_local(env, value)
            .and_then(|value| value.try_to_string(env))
            .map(Some)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeFrame(
        _env: EnvUnowned<'_>,
        _activity: JObject<'_>,
    ) {
        gpui_android::jni_frame();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeRefreshRate(
        _env: EnvUnowned<'_>,
        _activity: JObject<'_>,
        rate: f32,
    ) {
        gpui_android::jni_refresh_rate(rate);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeFirstFrameRendered(
        _env: EnvUnowned,
        _activity: JObject,
    ) -> jboolean {
        gpui_android::first_frame_rendered()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeCommitText<'local>(
        mut env: EnvUnowned<'local>,
        _activity: JObject<'local>,
        text: JString<'local>,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            gpui_android::jni_commit_text(text.try_to_string(env)?);
            Ok(())
        })
        .resolve::<LogErrorAndDefault>()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeSetComposingText<'local>(
        mut env: EnvUnowned<'local>,
        _activity: JObject<'local>,
        text: JString<'local>,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            gpui_android::jni_set_composing_text(text.try_to_string(env)?);
            Ok(())
        })
        .resolve::<LogErrorAndDefault>()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeFinishComposingText(
        _env: EnvUnowned,
        _activity: JObject,
    ) {
        gpui_android::jni_finish_composing_text();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeDeleteBackward(
        _env: EnvUnowned,
        _activity: JObject,
    ) {
        gpui_android::jni_delete_backward();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeInputState<'local>(
        mut env: EnvUnowned<'local>,
        _activity: JObject<'local>,
        revision: jlong,
        serial: jlong,
        text: JString<'local>,
        selection_start: jint,
        selection_end: jint,
        composing_start: jint,
        composing_end: jint,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            let text = text.try_to_string(env)?;
            let selection = selection_start.min(selection_end).max(0) as usize
                ..selection_start.max(selection_end).max(0) as usize;
            let marked = (composing_start >= 0 && composing_end > composing_start)
                .then_some(composing_start as usize..composing_end as usize);
            gpui_android::jni_input_state(
                revision as u64,
                serial as u64,
                gpui_android::TextInputState {
                    text,
                    selection,
                    marked,
                },
            );
            Ok(())
        })
        .resolve::<LogErrorAndDefault>()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeKeyEvent(
        _env: EnvUnowned,
        _activity: JObject,
        key_code: jint,
        down: jboolean,
        unicode_code_point: jint,
        meta_state: jint,
    ) {
        gpui_android::jni_key_event(key_code, down, unicode_code_point, meta_state);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeOnInsets(
        _env: EnvUnowned,
        _activity: JObject,
        left: jint,
        top: jint,
        right: jint,
        bottom: jint,
        ime_bottom: jint,
    ) {
        gpui_android::jni_on_insets(left, top, right, bottom, ime_bottom);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeOnBack(
        _env: EnvUnowned,
        _activity: JObject,
        enabled: jboolean,
    ) {
        if enabled {
            gpui_android::jni_on_back();
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeScrollCaptureSearch(
        _env: EnvUnowned,
        _activity: JObject,
        request: jlong,
    ) {
        gpui_android::jni_scroll_capture(gpui_android::ScrollCaptureRequest::Search {
            request: request as u64,
        });
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeScrollCaptureStart(
        _env: EnvUnowned,
        _activity: JObject,
    ) {
        gpui_android::jni_scroll_capture(gpui_android::ScrollCaptureRequest::Start);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeScrollCaptureImage(
        _env: EnvUnowned,
        _activity: JObject,
        request: jlong,
        top: jint,
    ) {
        gpui_android::jni_scroll_capture(gpui_android::ScrollCaptureRequest::Image {
            request: request as u64,
            top,
        });
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeScrollCaptureEnd(
        _env: EnvUnowned,
        _activity: JObject,
    ) {
        gpui_android::jni_scroll_capture(gpui_android::ScrollCaptureRequest::End);
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeNetworkChanged(
        _env: EnvUnowned,
        _activity: JObject,
    ) {
        crate::host::network_changed();
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeImagePicked<'local>(
        mut env: EnvUnowned<'local>,
        _activity: JObject<'local>,
        request_id: jlong,
        name: JObject<'local>,
        mime: JObject<'local>,
        bytes: JByteArray<'local>,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            let name = optional_string(env, name)?.unwrap_or_else(|| "image".into());
            let mime = optional_string(env, mime)?.unwrap_or_else(|| "image/jpeg".into());
            let bytes = env.convert_byte_array(&bytes)?;
            crate::host::deliver_image_picked(
                request_id as u64,
                tcode_client::host::PickedImage { name, mime, bytes },
            );
            Ok(())
        })
        .resolve::<LogErrorAndDefault>()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeImagePickFinished<'local>(
        mut env: EnvUnowned<'local>,
        _activity: JObject<'local>,
        request_id: jlong,
        status: jint,
        error: JObject<'local>,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            let error = optional_string(env, error)?;
            crate::host::deliver_images_picked(request_id as u64, status, error);
            Ok(())
        })
        .resolve::<LogErrorAndDefault>()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_com_tryanks_tcode_GpuiActivity_nativeQrScanCompleted<'local>(
        mut env: EnvUnowned<'local>,
        _activity: JObject<'local>,
        request_id: jlong,
        status: jint,
        value: JObject<'local>,
    ) {
        env.with_env(|env| -> jni::errors::Result<()> {
            let value = optional_string(env, value)
                .map_err(|error| log::error!("failed reading QR result: {error}"))
                .ok()
                .flatten();
            crate::host::deliver_result(request_id as u64, status, value);
            Ok(())
        })
        .resolve::<LogErrorAndDefault>()
    }
}
