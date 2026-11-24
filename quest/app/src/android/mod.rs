//! Android entry point and OpenXR main loop.

mod app;
mod audio;
mod egl;
mod gfx;
mod video;
mod xr;

use std::ffi::{CString, c_void};
use std::time::Duration;

use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent};
use anyhow::{Context, Result};
use log::{LevelFilter, error, info, warn};
use openxr as oxr;

use self::egl::Egl;
use self::xr::{Input, Layers, Passthrough, XrBase};

#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    android_logger::init_once(android_logger::Config::default().with_max_level(LevelFilter::Debug).with_tag("hyprvr"));
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        error!("PANIC: {info}\n{bt}");
    }));
    info!("hyprvr {} starting", env!("CARGO_PKG_VERSION"));
    match run(&app) {
        Ok(()) => info!("hyprvr exiting"),
        Err(e) => error!("fatal: {e:#}"),
    }
}

/// GL entry points: eglGetProcAddress, falling back to dlsym on libGLESv3.
fn gl_loader(egl: &Egl) -> impl FnMut(&str) -> *const c_void + '_ {
    let lib = unsafe { libc::dlopen(c"libGLESv3.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    move |name: &str| {
        let p = egl.proc_address(name);
        if !p.is_null() || lib.is_null() {
            return p;
        }
        let Ok(c) = CString::new(name) else { return std::ptr::null() };
        unsafe { libc::dlsym(lib, c.as_ptr()) as *const c_void }
    }
}

fn run(android: &AndroidApp) -> Result<()> {
    let egl = Egl::new().context("EGL setup")?;
    let gl = unsafe { glow::Context::from_loader_function(gl_loader(&egl)) };
    {
        use glow::HasContext;
        let (v, r) = unsafe { (gl.get_parameter_string(glow::VERSION), gl.get_parameter_string(glow::RENDERER)) };
        info!("GL: {v} / {r}");
    }

    let base = XrBase::new(android.vm_as_ptr(), android.activity_as_ptr()).context("OpenXR instance")?;
    let (session, mut waiter, mut stream) = base.create_session(&egl)?;
    let (world, _) = xr::world_space(&session)?;
    let view_space = session.create_reference_space(oxr::ReferenceSpaceType::VIEW, oxr::Posef::IDENTITY)?;
    let input = Input::new(&base, &session).context("actions")?;
    let passthrough = if base.exts.passthrough {
        match Passthrough::new(&base, &session) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!("passthrough unavailable: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let vcv = base.instance.enumerate_view_configuration_views(base.system, oxr::ViewConfigurationType::PRIMARY_STEREO)?;
    let (rec_w, rec_h) = (vcv[0].recommended_image_rect_width, vcv[0].recommended_image_rect_height);
    info!("recommended eye size {rec_w}x{rec_h}");

    let files = android.internal_data_path().unwrap_or_else(|| "/data/local/tmp/hyprvr".into());
    let _ = std::fs::create_dir_all(&files);
    info!("files dir {}", files.display());

    let mut app = app::App::new(app::Init {
        gl,
        egl: &egl,
        session: session.clone(),
        files,
        passthrough,
        eye_size: (rec_w, rec_h),
        cylinder: base.exts.cylinder,
    })?;

    let mut running = false;
    let mut quit = false;
    let mut ev_buf = oxr::EventDataBuffer::new();
    let mut layers = Layers::new(base.exts.layer_settings);
    while !quit {
        android.poll_events(Some(if running { Duration::ZERO } else { Duration::from_millis(50) }), |e| {
            if let PollEvent::Main(m) = e {
                match m {
                    MainEvent::Destroy => {
                        info!("activity destroyed");
                        quit = true;
                    }
                    MainEvent::Pause => info!("activity paused"),
                    MainEvent::Resume { .. } => info!("activity resumed"),
                    MainEvent::LowMemory => warn!("low memory"),
                    _ => {}
                }
            }
        });
        if let Ok(mut it) = android.input_events_iter() {
            while it.next(|_| InputStatus::Unhandled) {}
        }

        while let Some(event) = base.instance.poll_event(&mut ev_buf)? {
            use oxr::Event;
            match event {
                Event::SessionStateChanged(s) => {
                    info!("session state {:?}", s.state());
                    match s.state() {
                        oxr::SessionState::READY => {
                            session.begin(oxr::ViewConfigurationType::PRIMARY_STEREO)?;
                            xr::request_refresh_rate(&base, &session, 90.0);
                            running = true;
                            app.set_active(true);
                        }
                        oxr::SessionState::STOPPING => {
                            session.end()?;
                            running = false;
                            app.set_active(false);
                        }
                        oxr::SessionState::EXITING | oxr::SessionState::LOSS_PENDING => quit = true,
                        _ => {}
                    }
                }
                Event::InstanceLossPending(_) => quit = true,
                Event::ReferenceSpaceChangePending(r) => info!("reference space change pending: {:?}", r.reference_space_type()),
                Event::InteractionProfileChanged(_) => {
                    for h in ["/user/hand/left", "/user/hand/right"] {
                        if let Ok(p) = base.instance.string_to_path(h)
                            && let Ok(prof) = session.current_interaction_profile(p)
                        {
                            let name = if prof == oxr::Path::NULL { "none".into() } else { base.instance.path_to_string(prof).unwrap_or_default() };
                            info!("interaction profile {h}: {name}");
                        }
                    }
                }
                _ => {}
            }
        }
        if quit {
            break;
        }
        if !running {
            app.idle();
            continue;
        }

        let fs = waiter.wait()?;
        stream.begin()?;
        layers.clear();
        if fs.should_render {
            let time = fs.predicted_display_time;
            let (_, views) = session.locate_views(oxr::ViewConfigurationType::PRIMARY_STEREO, time, &world)?;
            let head = view_space.locate(&world, time)?;
            let hands = input.poll(&session, &world, time);
            let frame = app::Frame {
                views: &views,
                head_pos: xr::to_vec3(head.pose.position),
                head_rot: xr::to_quat(head.pose.orientation),
                head_valid: head.location_flags.contains(oxr::SpaceLocationFlags::POSITION_VALID),
                hands,
                world: &world,
                view_space: &view_space,
            };
            if let Err(e) = app.frame(&frame, &mut layers) {
                error!("frame: {e:#}");
                layers.clear();
            }
        }
        layers.submit(&session, fs.predicted_display_time)?;
    }
    drop(app);
    Ok(())
}
