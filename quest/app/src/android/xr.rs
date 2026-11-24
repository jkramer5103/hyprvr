//! OpenXR plumbing: instance/session creation, spaces, actions, swapchains, passthrough and
//! raw composition-layer submission.

use std::num::NonZeroU32;
use std::ptr;

use anyhow::{Context, Result, anyhow, bail};
use glam::{Quat, Vec2, Vec3};
use log::{info, warn};
use openxr as xr;
use openxr::sys;
use openxr::sys::Handle as _;

use super::egl::Egl;
use super::gfx::GL_SRGB8_ALPHA8;

pub type Session = xr::Session<xr::OpenGlEs>;

pub fn to_vec3(v: xr::Vector3f) -> Vec3 {
    Vec3::new(v.x, v.y, v.z)
}

pub fn to_quat(q: xr::Quaternionf) -> Quat {
    Quat::from_xyzw(q.x, q.y, q.z, q.w)
}

pub fn to_posef(p: Vec3, q: Quat) -> xr::Posef {
    xr::Posef {
        position: xr::Vector3f { x: p.x, y: p.y, z: p.z },
        orientation: xr::Quaternionf { x: q.x, y: q.y, z: q.z, w: q.w },
    }
}

pub struct Exts {
    pub cylinder: bool,
    pub passthrough: bool,
    pub refresh_rate: bool,
    pub layer_settings: bool,
    pub hand_interaction: bool,
}

pub struct XrBase {
    pub instance: xr::Instance,
    pub system: xr::SystemId,
    pub exts: Exts,
}

impl XrBase {
    pub fn new(vm: *mut std::ffi::c_void, activity: *mut std::ffi::c_void) -> Result<Self> {
        let platform = unsafe { xr::AndroidPlatformInfo::new(vm, activity) };
        let entry = unsafe { xr::Entry::load(&platform) }.map_err(|e| anyhow!("loading libopenxr_loader.so: {e}"))?;
        let avail = entry.enumerate_extensions().context("enumerate extensions")?;
        info!(
            "OpenXR extensions: gles={} cylinder={} passthrough={} refresh={} layer_settings={} hand_interaction={}",
            avail.khr_opengl_es_enable,
            avail.khr_composition_layer_cylinder,
            avail.fb_passthrough,
            avail.fb_display_refresh_rate,
            avail.fb_composition_layer_settings,
            avail.ext_hand_interaction
        );
        if !avail.khr_opengl_es_enable {
            bail!("runtime lacks XR_KHR_opengl_es_enable");
        }
        let mut want = xr::ExtensionSet::default();
        want.khr_opengl_es_enable = true;
        want.khr_android_create_instance = avail.khr_android_create_instance;
        want.khr_composition_layer_cylinder = avail.khr_composition_layer_cylinder;
        want.fb_passthrough = avail.fb_passthrough;
        want.fb_display_refresh_rate = avail.fb_display_refresh_rate;
        want.fb_composition_layer_settings = avail.fb_composition_layer_settings;
        want.ext_hand_interaction = avail.ext_hand_interaction;
        let app_info = xr::ApplicationInfo {
            application_name: "hyprvr",
            application_version: 1,
            engine_name: "hyprvr",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        };
        let instance = entry.create_instance(&app_info, &want, &[], &platform).context("xrCreateInstance")?;
        let props = instance.properties()?;
        info!("OpenXR runtime {} {}", props.runtime_name, props.runtime_version);
        let system = instance.system(xr::FormFactor::HEAD_MOUNTED_DISPLAY).context("xrGetSystem")?;
        let sp = instance.system_properties(system)?;
        info!("system: {} (max layers {})", sp.system_name, sp.graphics_properties.max_layer_count);
        Ok(XrBase {
            instance,
            system,
            exts: Exts {
                cylinder: want.khr_composition_layer_cylinder,
                passthrough: want.fb_passthrough,
                refresh_rate: want.fb_display_refresh_rate,
                layer_settings: want.fb_composition_layer_settings,
                hand_interaction: want.ext_hand_interaction,
            },
        })
    }

    pub fn create_session(&self, egl: &Egl) -> Result<(Session, xr::FrameWaiter, xr::FrameStream<xr::OpenGlEs>)> {
        let req = self.instance.graphics_requirements::<xr::OpenGlEs>(self.system)?;
        info!("GLES requirements: {:?} .. {:?}", req.min_api_version_supported, req.max_api_version_supported);
        let info = xr::opengles::SessionCreateInfo::Android {
            display: egl.display.as_ptr(),
            config: egl.config.as_ptr(),
            context: egl.context.as_ptr(),
        };
        unsafe { self.instance.create_session::<xr::OpenGlEs>(self.system, &info) }.context("xrCreateSession")
    }
}

/// Pick STAGE, else LOCAL_FLOOR, else LOCAL.
pub fn world_space(session: &Session) -> Result<(xr::Space, xr::ReferenceSpaceType)> {
    let avail = session.enumerate_reference_spaces()?;
    for ty in [xr::ReferenceSpaceType::STAGE, xr::ReferenceSpaceType::LOCAL_FLOOR, xr::ReferenceSpaceType::LOCAL] {
        if avail.contains(&ty) {
            let s = session.create_reference_space(ty, xr::Posef::IDENTITY)?;
            info!("world space: {ty:?}");
            return Ok((s, ty));
        }
    }
    bail!("no usable reference space")
}

pub fn request_refresh_rate(base: &XrBase, session: &Session, want: f32) {
    if !base.exts.refresh_rate {
        return;
    }
    let Some(f) = base.instance.exts().fb_display_refresh_rate else { return };
    unsafe {
        let mut n = 0u32;
        if (f.enumerate_display_refresh_rates)(session.as_raw(), 0, &mut n, ptr::null_mut()) != sys::Result::SUCCESS {
            return;
        }
        let mut rates = vec![0f32; n as usize];
        (f.enumerate_display_refresh_rates)(session.as_raw(), n, &mut n, rates.as_mut_ptr());
        info!("display refresh rates: {rates:?}");
        let pick = rates.iter().copied().find(|r| (*r - want).abs() < 0.5).or_else(|| rates.iter().copied().fold(None, |a: Option<f32>, r| Some(a.map_or(r, |a| a.max(r)))));
        if let Some(r) = pick {
            let res = (f.request_display_refresh_rate)(session.as_raw(), r);
            info!("requested {r} Hz: {res:?}");
        }
    }
}

/// Passthrough feature + one reconstruction layer, both running from creation (the
/// openxr crate's `Passthrough::start` wrapper pauses instead, so we use raw calls).
pub struct Passthrough {
    pub layer: sys::PassthroughLayerFB,
}

impl Passthrough {
    pub fn new(base: &XrBase, session: &Session) -> Result<Self> {
        let f = base.instance.exts().fb_passthrough.ok_or_else(|| anyhow!("XR_FB_passthrough not enabled"))?;
        unsafe {
            let mut pt = sys::PassthroughFB::NULL;
            let info = sys::PassthroughCreateInfoFB {
                ty: sys::PassthroughCreateInfoFB::TYPE,
                next: ptr::null(),
                flags: sys::PassthroughFlagsFB::IS_RUNNING_AT_CREATION,
            };
            let r = (f.create_passthrough)(session.as_raw(), &info, &mut pt);
            if r != sys::Result::SUCCESS {
                bail!("xrCreatePassthroughFB: {r:?}");
            }
            let mut layer = sys::PassthroughLayerFB::NULL;
            let linfo = sys::PassthroughLayerCreateInfoFB {
                ty: sys::PassthroughLayerCreateInfoFB::TYPE,
                next: ptr::null(),
                passthrough: pt,
                flags: sys::PassthroughFlagsFB::IS_RUNNING_AT_CREATION,
                purpose: sys::PassthroughLayerPurposeFB::RECONSTRUCTION,
            };
            let r = (f.create_passthrough_layer)(session.as_raw(), &linfo, &mut layer);
            if r != sys::Result::SUCCESS {
                bail!("xrCreatePassthroughLayerFB: {r:?}");
            }
            let r1 = (f.passthrough_start)(pt);
            let r2 = (f.passthrough_layer_resume)(layer);
            info!("passthrough ready (start {r1:?}, resume {r2:?})");
            Ok(Passthrough { layer })
        }
    }
}

/// A swapchain with its GL texture names.
pub struct Sc {
    pub sc: xr::Swapchain<xr::OpenGlEs>,
    pub images: Vec<u32>,
    pub w: u32,
    pub h: u32,
    pub released_once: bool,
}

pub fn pick_format(session: &Session) -> u32 {
    let formats = session.enumerate_swapchain_formats().unwrap_or_default();
    if formats.contains(&GL_SRGB8_ALPHA8) {
        GL_SRGB8_ALPHA8
    } else {
        warn!("GL_SRGB8_ALPHA8 swapchains unsupported; formats {formats:?}");
        formats.first().copied().unwrap_or(glow::RGBA8)
    }
}

impl Sc {
    pub fn new(session: &Session, format: u32, w: u32, h: u32) -> Result<Self> {
        let sc = session
            .create_swapchain(&xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT | xr::SwapchainUsageFlags::SAMPLED | xr::SwapchainUsageFlags::TRANSFER_DST,
                format,
                sample_count: 1,
                width: w,
                height: h,
                face_count: 1,
                array_size: 1,
                mip_count: 1,
            })
            .with_context(|| format!("swapchain {w}x{h}"))?;
        let images = sc.enumerate_images()?;
        Ok(Sc { sc, images, w, h, released_once: false })
    }

    pub fn acquire(&mut self) -> Result<glow::Texture> {
        let i = self.sc.acquire_image()?;
        self.sc.wait_image(xr::Duration::INFINITE)?;
        Ok(glow::NativeTexture(NonZeroU32::new(self.images[i as usize]).ok_or_else(|| anyhow!("null swapchain image"))?))
    }

    pub fn release(&mut self) {
        if let Err(e) = self.sc.release_image() {
            warn!("release swapchain image: {e}");
        } else {
            self.released_once = true;
        }
    }

    pub fn sub_image(&self, x: i32, y: i32, w: i32, h: i32) -> sys::SwapchainSubImage {
        sys::SwapchainSubImage {
            swapchain: self.sc.as_raw(),
            image_rect: xr::Rect2Di { offset: xr::Offset2Di { x, y }, extent: xr::Extent2Di { width: w, height: h } },
            image_array_index: 0,
        }
    }

    pub fn full(&self) -> sys::SwapchainSubImage {
        self.sub_image(0, 0, self.w as i32, self.h as i32)
    }
}

// ------------------------------------------------------------------ input

#[derive(Clone, Copy, Default, Debug)]
pub struct HandInput {
    pub active: bool,
    pub aim_pos: Vec3,
    pub aim_rot: Quat,
    pub select: f32,
    pub squeeze: f32,
    pub stick: Vec2,
    pub alt: bool,
    pub menu: bool,
    /// Grip (palm) pose; +X of the grip points out of the left palm / into the right palm.
    pub grip_valid: bool,
    pub grip_pos: Vec3,
    pub grip_rot: Quat,
    /// Tracked hand (not a controller).
    pub is_hand: bool,
}

pub struct Input {
    pub set: xr::ActionSet,
    aim: xr::Action<xr::Posef>,
    select: xr::Action<f32>,
    squeeze: xr::Action<f32>,
    stick: xr::Action<xr::Vector2f>,
    alt: xr::Action<bool>,
    menu: xr::Action<bool>,
    grip: xr::Action<xr::Posef>,
    hands: [xr::Path; 2],
    aim_spaces: [xr::Space; 2],
    grip_spaces: [xr::Space; 2],
    hand_profile: xr::Path,
}

impl Input {
    pub fn new(base: &XrBase, session: &Session) -> Result<Self> {
        let inst = &base.instance;
        let set = inst.create_action_set("main", "Main", 0)?;
        let hands = [inst.string_to_path("/user/hand/left")?, inst.string_to_path("/user/hand/right")?];
        let aim = set.create_action::<xr::Posef>("aim", "Aim", &hands)?;
        let select = set.create_action::<f32>("select", "Select", &hands)?;
        let squeeze = set.create_action::<f32>("squeeze", "Grab", &hands)?;
        let stick = set.create_action::<xr::Vector2f>("stick", "Thumbstick", &hands)?;
        let alt = set.create_action::<bool>("alt", "Right click", &hands)?;
        let menu = set.create_action::<bool>("menu", "Menu", &hands)?;
        let grip = set.create_action::<xr::Posef>("grip", "Grip", &hands)?;
        let p = |s: &str| inst.string_to_path(s);

        let touch = vec![
            xr::Binding::new(&aim, p("/user/hand/left/input/aim/pose")?),
            xr::Binding::new(&aim, p("/user/hand/right/input/aim/pose")?),
            xr::Binding::new(&select, p("/user/hand/left/input/trigger/value")?),
            xr::Binding::new(&select, p("/user/hand/right/input/trigger/value")?),
            xr::Binding::new(&squeeze, p("/user/hand/left/input/squeeze/value")?),
            xr::Binding::new(&squeeze, p("/user/hand/right/input/squeeze/value")?),
            xr::Binding::new(&stick, p("/user/hand/left/input/thumbstick")?),
            xr::Binding::new(&stick, p("/user/hand/right/input/thumbstick")?),
            xr::Binding::new(&alt, p("/user/hand/left/input/y/click")?),
            xr::Binding::new(&alt, p("/user/hand/right/input/b/click")?),
            xr::Binding::new(&menu, p("/user/hand/left/input/menu/click")?),
            xr::Binding::new(&grip, p("/user/hand/left/input/grip/pose")?),
            xr::Binding::new(&grip, p("/user/hand/right/input/grip/pose")?),
        ];
        inst.suggest_interaction_profile_bindings(p("/interaction_profiles/oculus/touch_controller")?, &touch)
            .context("touch bindings")?;

        let simple = vec![
            xr::Binding::new(&aim, p("/user/hand/left/input/aim/pose")?),
            xr::Binding::new(&aim, p("/user/hand/right/input/aim/pose")?),
            xr::Binding::new(&select, p("/user/hand/left/input/select/click")?),
            xr::Binding::new(&select, p("/user/hand/right/input/select/click")?),
            xr::Binding::new(&menu, p("/user/hand/left/input/menu/click")?),
            xr::Binding::new(&menu, p("/user/hand/right/input/menu/click")?),
        ];
        if let Err(e) = inst.suggest_interaction_profile_bindings(p("/interaction_profiles/khr/simple_controller")?, &simple) {
            warn!("simple controller bindings: {e}");
        }

        if base.exts.hand_interaction {
            let hand = vec![
                xr::Binding::new(&aim, p("/user/hand/left/input/aim/pose")?),
                xr::Binding::new(&aim, p("/user/hand/right/input/aim/pose")?),
                xr::Binding::new(&select, p("/user/hand/left/input/pinch_ext/value")?),
                xr::Binding::new(&select, p("/user/hand/right/input/pinch_ext/value")?),
                xr::Binding::new(&squeeze, p("/user/hand/left/input/grasp_ext/value")?),
                xr::Binding::new(&squeeze, p("/user/hand/right/input/grasp_ext/value")?),
                xr::Binding::new(&grip, p("/user/hand/left/input/grip/pose")?),
                xr::Binding::new(&grip, p("/user/hand/right/input/grip/pose")?),
            ];
            let profile = p("/interaction_profiles/ext/hand_interaction_ext")?;
            // Meta may route its left-hand system "menu" gesture here; not part of the EXT spec,
            // so fall back to the spec bindings if the runtime rejects the path.
            let mut with_menu = hand.clone();
            with_menu.push(xr::Binding::new(&menu, p("/user/hand/left/input/menu/click")?));
            match inst.suggest_interaction_profile_bindings(profile, &with_menu) {
                Ok(()) => info!("hand interaction bindings suggested (with menu)"),
                Err(e) => {
                    info!("hand interaction: menu binding rejected ({e}); using spec bindings");
                    match inst.suggest_interaction_profile_bindings(profile, &hand) {
                        Ok(()) => info!("hand interaction bindings suggested"),
                        Err(e) => warn!("hand interaction bindings: {e}"),
                    }
                }
            }
        }

        session.attach_action_sets(&[&set])?;
        let aim_spaces = [
            aim.create_space(session, hands[0], xr::Posef::IDENTITY)?,
            aim.create_space(session, hands[1], xr::Posef::IDENTITY)?,
        ];
        let grip_spaces = [
            grip.create_space(session, hands[0], xr::Posef::IDENTITY)?,
            grip.create_space(session, hands[1], xr::Posef::IDENTITY)?,
        ];
        let hand_profile = p("/interaction_profiles/ext/hand_interaction_ext")?;
        Ok(Input { set, aim, select, squeeze, stick, alt, menu, grip, hands, aim_spaces, grip_spaces, hand_profile })
    }

    pub fn poll(&self, session: &Session, world: &xr::Space, time: xr::Time) -> [HandInput; 2] {
        if let Err(e) = session.sync_actions(&[xr::ActiveActionSet::new(&self.set)]) {
            // SESSION_NOT_FOCUSED is expected while the system menu is open.
            if e != xr::sys::Result::SESSION_NOT_FOCUSED {
                warn!("xrSyncActions: {e}");
            }
            return [HandInput::default(); 2];
        }
        let mut out = [HandInput::default(); 2];
        for (i, h) in out.iter_mut().enumerate() {
            let path = self.hands[i];
            if let Ok(loc) = self.aim_spaces[i].locate(world, time) {
                let f = loc.location_flags;
                if f.contains(xr::SpaceLocationFlags::POSITION_VALID) && f.contains(xr::SpaceLocationFlags::ORIENTATION_VALID) {
                    h.active = self.aim.is_active(session, path).unwrap_or(false);
                    h.aim_pos = to_vec3(loc.pose.position);
                    h.aim_rot = to_quat(loc.pose.orientation);
                }
            }
            h.select = self.select.state(session, path).map(|s| s.current_state).unwrap_or(0.0);
            h.squeeze = self.squeeze.state(session, path).map(|s| s.current_state).unwrap_or(0.0);
            h.stick = self.stick.state(session, path).map(|s| Vec2::new(s.current_state.x, s.current_state.y)).unwrap_or_default();
            h.alt = self.alt.state(session, path).map(|s| s.current_state).unwrap_or(false);
            h.menu = self.menu.state(session, path).map(|s| s.current_state).unwrap_or(false);
            if self.grip.is_active(session, path).unwrap_or(false)
                && let Ok(loc) = self.grip_spaces[i].locate(world, time)
            {
                let f = loc.location_flags;
                if f.contains(xr::SpaceLocationFlags::POSITION_VALID) && f.contains(xr::SpaceLocationFlags::ORIENTATION_VALID) {
                    h.grip_valid = true;
                    h.grip_pos = to_vec3(loc.pose.position);
                    h.grip_rot = to_quat(loc.pose.orientation);
                }
            }
            h.is_hand = session.current_interaction_profile(path).map(|p| p == self.hand_profile).unwrap_or(false);
        }
        out
    }
}

// ------------------------------------------------------------------ layers

/// Owns the raw layer structs for one frame so their addresses stay stable until xrEndFrame.
#[derive(Default)]
pub struct Layers {
    settings: Option<Box<sys::CompositionLayerSettingsFB>>,
    /// Pointers into these are only taken in `submit`, after all pushes.
    items: Vec<LayerBox>,
}

#[allow(clippy::large_enum_variant)]
enum LayerBox {
    Passthrough(sys::CompositionLayerPassthroughFB),
    /// The boxed views are referenced by pointer from the projection struct.
    Projection(sys::CompositionLayerProjection, #[allow(dead_code)] Box<[sys::CompositionLayerProjectionView; 2]>),
    Quad(sys::CompositionLayerQuad),
    Cylinder(sys::CompositionLayerCylinderKHR),
}

impl Layers {
    pub fn new(layer_settings: bool) -> Self {
        let settings = layer_settings.then(|| {
            Box::new(sys::CompositionLayerSettingsFB {
                ty: sys::CompositionLayerSettingsFB::TYPE,
                next: ptr::null(),
                layer_flags: sys::CompositionLayerSettingsFlagsFB::NORMAL_SUPER_SAMPLING | sys::CompositionLayerSettingsFlagsFB::NORMAL_SHARPENING,
            })
        });
        Layers { settings, items: Vec::new() }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    fn settings_ptr(&self, sharpen: bool) -> *const std::ffi::c_void {
        match (&self.settings, sharpen) {
            (Some(s), true) => &**s as *const _ as *const _,
            _ => ptr::null(),
        }
    }

    pub fn passthrough(&mut self, layer: sys::PassthroughLayerFB, _space: &xr::Space) {
        self.items.push(LayerBox::Passthrough(sys::CompositionLayerPassthroughFB {
            ty: sys::CompositionLayerPassthroughFB::TYPE,
            next: ptr::null(),
            flags: sys::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
            space: sys::Space::NULL, // runtime rejects a space on passthrough layers
            layer_handle: layer,
        }));
    }

    pub fn projection(&mut self, space: &xr::Space, views: &[xr::View], sc: &Sc, eye_w: i32, eye_h: i32, blend: bool) {
        let mk = |i: usize| sys::CompositionLayerProjectionView {
            ty: sys::CompositionLayerProjectionView::TYPE,
            next: ptr::null(),
            pose: views[i].pose,
            fov: views[i].fov,
            sub_image: sc.sub_image(i as i32 * eye_w, 0, eye_w, eye_h),
        };
        let pv = Box::new([mk(0), mk(1)]);
        let proj = sys::CompositionLayerProjection {
            ty: sys::CompositionLayerProjection::TYPE,
            next: ptr::null(),
            layer_flags: if blend {
                sys::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA
            } else {
                sys::CompositionLayerFlags::EMPTY
            },
            space: space.as_raw(),
            view_count: 2,
            views: pv.as_ptr(),
        };
        self.items.push(LayerBox::Projection(proj, pv));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn quad(&mut self, space: &xr::Space, sub: sys::SwapchainSubImage, pos: Vec3, rot: Quat, w: f32, h: f32, blend: bool, sharpen: bool) {
        self.items.push(LayerBox::Quad(sys::CompositionLayerQuad {
            ty: sys::CompositionLayerQuad::TYPE,
            next: self.settings_ptr(sharpen),
            layer_flags: if blend { sys::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA } else { sys::CompositionLayerFlags::EMPTY },
            space: space.as_raw(),
            eye_visibility: sys::EyeVisibility::BOTH,
            sub_image: sub,
            pose: to_posef(pos, rot),
            size: xr::Extent2Df { width: w, height: h },
        }));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cylinder(&mut self, space: &xr::Space, sub: sys::SwapchainSubImage, axis: Vec3, rot: Quat, radius: f32, angle: f32, aspect: f32) {
        self.items.push(LayerBox::Cylinder(sys::CompositionLayerCylinderKHR {
            ty: sys::CompositionLayerCylinderKHR::TYPE,
            next: self.settings_ptr(true),
            layer_flags: sys::CompositionLayerFlags::EMPTY,
            space: space.as_raw(),
            eye_visibility: sys::EyeVisibility::BOTH,
            sub_image: sub,
            pose: to_posef(axis, rot),
            radius,
            central_angle: angle,
            aspect_ratio: aspect,
        }));
    }

    /// xrEndFrame with the collected layers.
    pub fn submit(&self, session: &Session, time: xr::Time) -> Result<()> {
        let ptrs: Vec<*const sys::CompositionLayerBaseHeader> = self
            .items
            .iter()
            .map(|b| match b {
                LayerBox::Passthrough(l) => l as *const _ as *const sys::CompositionLayerBaseHeader,
                LayerBox::Projection(l, _) => l as *const _ as *const _,
                LayerBox::Quad(l) => l as *const _ as *const _,
                LayerBox::Cylinder(l) => l as *const _ as *const _,
            })
            .collect();
        let info = sys::FrameEndInfo {
            ty: sys::FrameEndInfo::TYPE,
            next: ptr::null(),
            display_time: time,
            environment_blend_mode: xr::EnvironmentBlendMode::OPAQUE,
            layer_count: ptrs.len() as u32,
            layers: ptrs.as_ptr(),
        };
        let r = unsafe { (session.instance().fp().end_frame)(session.as_raw(), &info) };
        if r.into_raw() < 0 {
            bail!("xrEndFrame: {r:?}");
        }
        Ok(())
    }
}
