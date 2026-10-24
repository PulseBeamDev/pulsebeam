//! Stable, role-bound SDK DeviceInfo products, distinct from discovery snapshots.
use crate::{CameraControlError, CameraSettings, VideoRotation, ffi, peer::FactoryInner};
use std::rc::Rc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraDeviceName {
    pub name_utf8: Vec<u8>,
    pub unique_id_utf8: Vec<u8>,
    pub product_id_utf8: Option<Vec<u8>>,
}
/// Native buffer sizes, including room for the terminating NUL. Too-small
/// buffers preserve the native error; output bytes exclude the terminator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CameraDeviceNameCapacities {
    pub name: u32,
    pub unique_id: u32,
    pub product_id: Option<u32>,
}
impl Default for CameraDeviceNameCapacities {
    fn default() -> Self {
        Self {
            name: 1024,
            unique_id: 1024,
            product_id: Some(1024),
        }
    }
}
pub struct CameraDeviceInfo {
    native: cxx::UniquePtr<ffi::NativeCameraDeviceInfo>,
    _factory: Rc<FactoryInner>,
    _options: Option<crate::CameraCaptureOptions>,
}
impl CameraDeviceInfo {
    pub(crate) fn new(
        native: cxx::UniquePtr<ffi::NativeCameraDeviceInfo>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        Self {
            native,
            _factory: factory,
            _options: None,
        }
    }
    pub(crate) fn with_options(mut self, options: crate::CameraCaptureOptions) -> Self {
        self._options = Some(options);
        self
    }
    fn query(
        &mut self,
        request: ffi::FfiCameraInfoRequest,
    ) -> Result<ffi::FfiCameraInfoReply, CameraControlError> {
        self._factory
            .check_control_call()
            .map_err(CameraControlError::Binding)?;
        let reply = ffi::camera_info_query(
            self.native.as_ref().expect("owned SDK device info"),
            &request,
        );
        CameraControlError::check(reply.status, reply.native_result)?;
        Ok(reply)
    }
    pub fn number_of_devices(&mut self) -> Result<u32, CameraControlError> {
        Ok(self
            .query(ffi::FfiCameraInfoRequest {
                kind: 0,
                ..Default::default()
            })?
            .count)
    }
    pub fn device_name(
        &mut self,
        index: u32,
        capacities: CameraDeviceNameCapacities,
    ) -> Result<CameraDeviceName, CameraControlError> {
        let reply = self.query(ffi::FfiCameraInfoRequest {
            kind: 1,
            index,
            name_capacity: capacities.name,
            unique_capacity: capacities.unique_id,
            product_present: capacities.product_id.is_some(),
            product_capacity: capacities.product_id.unwrap_or_default(),
            ..Default::default()
        })?;
        Ok(CameraDeviceName {
            name_utf8: reply.name,
            unique_id_utf8: reply.unique_id,
            product_id_utf8: capacities.product_id.map(|_| reply.product_id),
        })
    }
    pub fn number_of_capabilities(&mut self, device_id: &str) -> Result<u32, CameraControlError> {
        Ok(self
            .query(ffi::FfiCameraInfoRequest {
                kind: 2,
                id: device_id.into(),
                ..Default::default()
            })?
            .count)
    }
    pub fn capability(
        &mut self,
        device_id: &str,
        index: u32,
    ) -> Result<CameraSettings, CameraControlError> {
        let reply = self.query(ffi::FfiCameraInfoRequest {
            kind: 3,
            id: device_id.into(),
            index,
            ..Default::default()
        })?;
        Ok(CameraSettings::from_native(reply.settings))
    }
    pub fn orientation(&mut self, device_id: &str) -> Result<VideoRotation, CameraControlError> {
        Ok(VideoRotation::from_degrees(
            self.query(ffi::FfiCameraInfoRequest {
                kind: 4,
                id: device_id.into(),
                ..Default::default()
            })?
            .rotation,
        ))
    }
    pub fn best_matched_capability(
        &mut self,
        device_id: &str,
        requested: &CameraSettings,
    ) -> Result<(u32, CameraSettings), CameraControlError> {
        let reply = self.query(ffi::FfiCameraInfoRequest {
            kind: 5,
            id: device_id.into(),
            settings: requested.native(),
            ..Default::default()
        })?;
        Ok((reply.count, CameraSettings::from_native(reply.settings)))
    }
    /// Calls the real SDK operation. Pinned Linux V4L2 always returns -1 and
    /// displays no dialog. A parent-window pointer is inapplicable on Linux.
    pub fn display_settings_dialog(
        &mut self,
        device_id: &str,
        title: &str,
        x: u32,
        y: u32,
    ) -> Result<(), CameraControlError> {
        self.query(ffi::FfiCameraInfoRequest {
            kind: 6,
            id: device_id.into(),
            title: title.into(),
            x,
            y,
            ..Default::default()
        })
        .map(|_| ())
    }
    pub fn close(&mut self) -> Result<(), CameraControlError> {
        self._factory
            .check_control_call()
            .map_err(CameraControlError::Binding)?;
        ffi::camera_info_close(self.native.as_ref().expect("owned SDK device info"));
        Ok(())
    }
}
