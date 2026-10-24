//! Owning Linux SDK desktop options. Local handles never enter an actor.
use crate::{
    ffi,
    peer::{FactoryInner, PeerError, PeerErrorKind},
};
use std::rc::Rc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopCaptureKind {
    Screen,
    Window,
    Generic,
}
impl DesktopCaptureKind {
    pub(crate) fn tag(self) -> u8 {
        match self {
            Self::Screen => 0,
            Self::Window => 1,
            Self::Generic => 2,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipeWireCaptureSettings {
    pub allow: bool,
    pub width: u32,
    pub height: u32,
    pub use_damage_region: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopCaptureSettings {
    pub use_update_notifications: bool,
    /// The SDK stores this on Linux but only Windows capturers consume it.
    pub disable_effects: bool,
    pub detect_updated_region: bool,
    pub prefer_cursor_embedded: bool,
    /// None means leave PipeWire settings unchanged on write. On read, None
    /// means the artifact does not include that SDK feature.
    pub pipewire: Option<PipeWireCaptureSettings>,
}
/// An actual SDK options value. Copying it shares native resource ownership,
/// but changing the value does not reconfigure previously created capturers.
/// All operations and final native resource releases use its retained SDK role.
pub struct DesktopCaptureOptions {
    pub(crate) native: cxx::UniquePtr<ffi::NativeDesktopOptions>,
    pub(crate) factory: Rc<FactoryInner>,
    clock: crate::Clock,
}
pub struct DesktopXDisplay {
    lease: DesktopCaptureOptions,
}
pub struct DesktopScreenCastStream {
    lease: DesktopCaptureOptions,
}

fn error(kind: PeerErrorKind, message: &str) -> PeerError {
    PeerError {
        kind,
        message: message.into(),
    }
}
impl DesktopCaptureOptions {
    pub(crate) fn new(
        native: cxx::UniquePtr<ffi::NativeDesktopOptions>,
        factory: Rc<FactoryInner>,
        clock: crate::Clock,
    ) -> Self {
        Self {
            native,
            factory,
            clock,
        }
    }
    fn own(&self, native: cxx::UniquePtr<ffi::NativeDesktopOptions>) -> Self {
        Self::new(native, self.factory.clone(), self.clock.clone())
    }
    pub(crate) fn value(&self) -> &ffi::NativeDesktopOptions {
        self.native.as_ref().expect("validated SDK desktop options")
    }
    fn check(&self) -> Result<(), PeerError> {
        self.factory.check_control_call()
    }
    pub(crate) fn compatible(&self, other: &Rc<FactoryInner>) -> Result<(), PeerError> {
        self.check()?;
        if !Rc::ptr_eq(&self.factory, other) {
            return Err(error(
                PeerErrorKind::InvalidParameter,
                "desktop resources belong to another factory role",
            ));
        }
        Ok(())
    }
    pub fn try_clone(&self) -> Result<Self, PeerError> {
        self.check()?;
        Ok(self.own(ffi::desktop_options_clone(self.value())))
    }
    pub fn assign_from(&mut self, other: &Self) -> Result<(), PeerError> {
        self.compatible(&other.factory)?;
        self.native = ffi::desktop_options_clone(other.value());
        self.clock = other.clock.clone();
        Ok(())
    }
    pub fn settings(&self) -> Result<DesktopCaptureSettings, PeerError> {
        self.check()?;
        let f = ffi::desktop_options_read(self.value());
        Ok(DesktopCaptureSettings {
            use_update_notifications: f.use_update_notifications,
            disable_effects: f.disable_effects,
            detect_updated_region: f.detect_updated_region,
            prefer_cursor_embedded: f.prefer_cursor_embedded,
            pipewire: f.pipewire_supported.then_some(PipeWireCaptureSettings {
                allow: f.allow_pipewire,
                width: f.width,
                height: f.height,
                use_damage_region: f.pipewire_use_damage_region,
            }),
        })
    }
    pub fn set_settings(&mut self, settings: &DesktopCaptureSettings) -> Result<(), PeerError> {
        self.check()?;
        let mut f = ffi::FfiDesktopOptions {
            use_update_notifications: settings.use_update_notifications,
            disable_effects: settings.disable_effects,
            detect_updated_region: settings.detect_updated_region,
            prefer_cursor_embedded: settings.prefer_cursor_embedded,
            ..Default::default()
        };
        if let Some(p) = &settings.pipewire {
            f.pipewire_supported = true;
            f.allow_pipewire = p.allow;
            f.width = p.width;
            f.height = p.height;
            f.pipewire_use_damage_region = p.use_damage_region;
        }
        Self::result(ffi::desktop_options_write(self.value(), &f))
    }
    fn result(status: u8) -> Result<(), PeerError> {
        match status {
            0 => Ok(()),
            2 => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "SDK desktop feature is not compiled in this artifact",
            )),
            _ => Err(error(
                PeerErrorKind::InvalidParameter,
                "incompatible SDK desktop resource",
            )),
        }
    }
    /// Read the actual options clock, either selected Environment or SDK global
    /// clock as requested at construction. This is not a synthesized timestamp.
    pub fn clock_time_us(&self) -> Result<i64, PeerError> {
        self.check()?;
        Ok(ffi::desktop_options_time_us(self.value()))
    }
    /// Own the exact native options clock, retaining its leases independently.
    pub fn clock(&self) -> Result<crate::Clock, PeerError> {
        self.check()?;
        if !ffi::desktop_options_clock_matches(self.value(), self.clock.native()) {
            return Err(error(
                PeerErrorKind::Internal,
                "SDK options clock identity changed",
            ));
        }
        Ok(self.clock.clone())
    }
    pub fn is_running_under_wayland(&self) -> Result<bool, PeerError> {
        self.check()?;
        match ffi::desktop_is_wayland(self.value()) {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(error(
                PeerErrorKind::UnsupportedOperation,
                "native desktop backend detection is not compiled",
            )),
        }
    }
    pub fn supports_x11(&self) -> Result<bool, PeerError> {
        self.check()?;
        Ok(ffi::desktop_options_read(self.value()).x11_supported)
    }
    pub fn has_full_screen_window_detector(&self) -> Result<bool, PeerError> {
        self.check()?;
        Ok(ffi::desktop_options_read(self.value()).has_full_screen_window_detector)
    }
    pub fn x_display(&self) -> Result<Option<DesktopXDisplay>, PeerError> {
        self.check()?;
        let native = ffi::desktop_options_resource(self.value(), 0);
        Ok((!native.is_null()).then(|| DesktopXDisplay {
            lease: self.own(native),
        }))
    }
    pub fn screencast_stream(&self) -> Result<Option<DesktopScreenCastStream>, PeerError> {
        self.check()?;
        let native = ffi::desktop_options_resource(self.value(), 1);
        Ok((!native.is_null()).then(|| DesktopScreenCastStream {
            lease: self.own(native),
        }))
    }
    pub fn create_x_display(&self, name: &str) -> Result<Option<DesktopXDisplay>, PeerError> {
        self.check()?;
        if !self.supports_x11()? {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "X11 is not compiled in this artifact",
            ));
        }
        if name.contains('\0') {
            return Err(error(
                PeerErrorKind::InvalidParameter,
                "X11 display name contains NUL",
            ));
        }
        let native = ffi::desktop_resource_create(self.value(), 0, name);
        Ok((!native.is_null()).then(|| DesktopXDisplay {
            lease: self.own(native),
        }))
    }
    pub fn create_screencast_stream(&self) -> Result<DesktopScreenCastStream, PeerError> {
        self.check()?;
        if !ffi::desktop_options_read(self.value()).pipewire_supported {
            return Err(error(
                PeerErrorKind::UnsupportedOperation,
                "PipeWire is not compiled in this artifact",
            ));
        }
        let native = ffi::desktop_resource_create(self.value(), 1, "");
        if native.is_null() {
            return Err(error(
                PeerErrorKind::Operation,
                "SDK screencast stream creation failed",
            ));
        }
        Ok(DesktopScreenCastStream {
            lease: self.own(native),
        })
    }
    pub fn set_x_display(&mut self, display: Option<&DesktopXDisplay>) -> Result<(), PeerError> {
        self.set_resource(0, display.map(|d| &d.lease))
    }
    pub fn set_screencast_stream(
        &mut self,
        stream: Option<&DesktopScreenCastStream>,
    ) -> Result<(), PeerError> {
        self.set_resource(1, stream.map(|s| &s.lease))
    }
    fn set_resource(&mut self, kind: u8, resource: Option<&Self>) -> Result<(), PeerError> {
        self.check()?;
        if let Some(r) = resource {
            self.compatible(&r.factory)?;
        }
        Self::result(ffi::desktop_options_set_resource(
            self.value(),
            kind,
            resource.is_some(),
            resource.unwrap_or(self).value(),
        ))
    }
}
impl DesktopXDisplay {
    pub fn try_clone(&self) -> Result<Self, PeerError> {
        Ok(Self {
            lease: self.lease.try_clone()?,
        })
    }
    pub fn same_native_resource(&self, other: &Self) -> Result<bool, PeerError> {
        self.lease.compatible(&other.lease.factory)?;
        Ok(ffi::desktop_resource_equal(
            self.lease.value(),
            other.lease.value(),
            0,
        ))
    }
}
impl DesktopScreenCastStream {
    pub fn try_clone(&self) -> Result<Self, PeerError> {
        Ok(Self {
            lease: self.lease.try_clone()?,
        })
    }
    pub fn same_native_resource(&self, other: &Self) -> Result<bool, PeerError> {
        self.lease.compatible(&other.lease.factory)?;
        Ok(ffi::desktop_resource_equal(
            self.lease.value(),
            other.lease.value(),
            1,
        ))
    }
}
