//! Explicit opt-in physical capture inside the actor's captive graph.
use super::*;

impl<R: SessionRuntime> Session<R> {
    pub fn create_camera_options(&mut self) -> Result<SessionCameraOptionsId, PeerError> {
        self.check_open()?;
        let options = self.factory.create_camera_options()?;
        let id = SessionCameraOptionsId(next_id()?);
        self.camera_options.insert(id, options);
        Ok(id)
    }
    fn camera_options(
        &self,
        id: SessionCameraOptionsId,
    ) -> Result<&crate::CameraCaptureOptions, PeerError> {
        self.check_open()?;
        self.camera_options.get(&id).ok_or_else(missing)
    }
    pub fn clone_camera_options(
        &mut self,
        id: SessionCameraOptionsId,
    ) -> Result<SessionCameraOptionsId, PeerError> {
        let options = self.camera_options(id)?.try_clone()?;
        let id = SessionCameraOptionsId(next_id()?);
        self.camera_options.insert(id, options);
        Ok(id)
    }
    pub fn camera_options_settings(
        &self,
        id: SessionCameraOptionsId,
    ) -> Result<crate::CameraOptionSettings, PeerError> {
        self.camera_options(id)?.settings()
    }
    pub fn set_camera_options_settings(
        &mut self,
        id: SessionCameraOptionsId,
        settings: crate::CameraOptionSettings,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.camera_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .set_settings(settings)
    }
    #[cfg(unix)]
    pub fn set_camera_options_pipewire_fd(
        &mut self,
        id: SessionCameraOptionsId,
        fd: std::os::fd::BorrowedFd<'_>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.camera_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .set_pipewire_fd(fd)
    }
    #[cfg(unix)]
    pub fn clear_camera_options_pipewire_fd(
        &mut self,
        id: SessionCameraOptionsId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.camera_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .clear_pipewire_fd()
    }
    pub fn initialize_camera_options(
        &mut self,
        id: SessionCameraOptionsId,
        observer: Option<&crate::CameraInitializationObserverHandle>,
    ) -> Result<crate::CameraInitializationStatus, PeerError> {
        self.check_open()?;
        self.camera_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .initialize(observer)
    }
    pub fn close_camera_options(&mut self, id: SessionCameraOptionsId) -> Result<(), PeerError> {
        self.check_open()?;
        self.camera_options.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn create_camera_device_info_with_options(
        &mut self,
        options: SessionCameraOptionsId,
    ) -> Result<Option<SessionCameraDeviceInfoId>, PeerError> {
        self.check_open()?;
        let Some(info) = self
            .factory
            .create_camera_device_info_with_options(self.camera_options(options)?)?
        else {
            return Ok(None);
        };
        let id = SessionCameraDeviceInfoId(next_id()?);
        self.camera_device_info.insert(id, info);
        Ok(Some(id))
    }
    pub fn create_camera_with_options(
        &mut self,
        options: SessionCameraOptionsId,
        device_id: &str,
    ) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let capture = self
            .factory
            .create_camera_with_options(self.camera_options(options)?, device_id)?;
        let id = SessionSourceId(next_id()?);
        self.sources.insert(id, Source::Camera(capture));
        Ok(id)
    }
    pub fn create_camera_device_info(
        &mut self,
    ) -> Result<Option<SessionCameraDeviceInfoId>, PeerError> {
        self.check_open()?;
        let Some(info) = self.factory.create_camera_device_info()? else {
            return Ok(None);
        };
        let id = SessionCameraDeviceInfoId(next_id()?);
        self.camera_device_info.insert(id, info);
        Ok(Some(id))
    }
    fn camera_device_info(
        &mut self,
        id: SessionCameraDeviceInfoId,
    ) -> Result<&mut crate::CameraDeviceInfo, crate::CameraControlError> {
        self.check_open()
            .map_err(crate::CameraControlError::Binding)?;
        self.camera_device_info
            .get_mut(&id)
            .ok_or_else(|| crate::CameraControlError::Binding(missing()))
    }
    pub fn camera_info_number_of_devices(
        &mut self,
        id: SessionCameraDeviceInfoId,
    ) -> Result<u32, crate::CameraControlError> {
        self.camera_device_info(id)?.number_of_devices()
    }
    pub fn camera_info_device_name(
        &mut self,
        id: SessionCameraDeviceInfoId,
        index: u32,
        capacities: crate::CameraDeviceNameCapacities,
    ) -> Result<crate::CameraDeviceName, crate::CameraControlError> {
        self.camera_device_info(id)?.device_name(index, capacities)
    }
    pub fn camera_info_number_of_capabilities(
        &mut self,
        id: SessionCameraDeviceInfoId,
        device: &str,
    ) -> Result<u32, crate::CameraControlError> {
        self.camera_device_info(id)?.number_of_capabilities(device)
    }
    pub fn camera_info_capability(
        &mut self,
        id: SessionCameraDeviceInfoId,
        device: &str,
        index: u32,
    ) -> Result<crate::CameraSettings, crate::CameraControlError> {
        self.camera_device_info(id)?.capability(device, index)
    }
    pub fn camera_info_orientation(
        &mut self,
        id: SessionCameraDeviceInfoId,
        device: &str,
    ) -> Result<crate::VideoRotation, crate::CameraControlError> {
        self.camera_device_info(id)?.orientation(device)
    }
    pub fn camera_info_best_matched_capability(
        &mut self,
        id: SessionCameraDeviceInfoId,
        device: &str,
        requested: &crate::CameraSettings,
    ) -> Result<(u32, crate::CameraSettings), crate::CameraControlError> {
        self.camera_device_info(id)?
            .best_matched_capability(device, requested)
    }
    pub fn camera_info_display_settings_dialog(
        &mut self,
        id: SessionCameraDeviceInfoId,
        device: &str,
        title: &str,
        x: u32,
        y: u32,
    ) -> Result<(), crate::CameraControlError> {
        self.camera_device_info(id)?
            .display_settings_dialog(device, title, x, y)
    }
    pub fn close_camera_device_info(
        &mut self,
        id: SessionCameraDeviceInfoId,
    ) -> Result<(), crate::CameraControlError> {
        self.camera_device_info(id)?.close()?;
        self.camera_device_info.remove(&id);
        Ok(())
    }
    pub fn create_desktop_options(
        &mut self,
        defaults: bool,
        selected_environment: bool,
    ) -> Result<SessionDesktopOptionsId, PeerError> {
        self.check_open()?;
        let options = self
            .factory
            .desktop_capture_options(defaults, selected_environment)?;
        let id = SessionDesktopOptionsId(next_id()?);
        self.desktop_options.insert(id, options);
        Ok(id)
    }
    fn desktop_options(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<&crate::DesktopCaptureOptions, PeerError> {
        self.check_open()?;
        self.desktop_options.get(&id).ok_or_else(missing)
    }
    pub fn clone_desktop_options(
        &mut self,
        source: SessionDesktopOptionsId,
    ) -> Result<SessionDesktopOptionsId, PeerError> {
        let options = self.desktop_options(source)?.try_clone()?;
        let id = SessionDesktopOptionsId(next_id()?);
        self.desktop_options.insert(id, options);
        Ok(id)
    }
    pub fn desktop_options_settings(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<crate::DesktopCaptureSettings, PeerError> {
        self.desktop_options(id)?.settings()
    }
    pub fn set_desktop_options_settings(
        &mut self,
        id: SessionDesktopOptionsId,
        settings: &crate::DesktopCaptureSettings,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.desktop_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .set_settings(settings)
    }
    pub fn desktop_options_clock(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<crate::Clock, PeerError> {
        self.desktop_options(id)?.clock()
    }
    pub fn is_desktop_running_under_wayland(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<bool, PeerError> {
        self.desktop_options(id)?.is_running_under_wayland()
    }
    pub fn desktop_options_supports_x11(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<bool, PeerError> {
        self.desktop_options(id)?.supports_x11()
    }
    pub fn desktop_options_has_full_screen_detector(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<bool, PeerError> {
        self.desktop_options(id)?.has_full_screen_window_detector()
    }
    pub fn desktop_options_clock_time_us(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<i64, PeerError> {
        self.desktop_options(id)?.clock_time_us()
    }
    pub fn desktop_options_has_x_display(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<bool, PeerError> {
        Ok(self.desktop_options(id)?.x_display()?.is_some())
    }
    pub fn desktop_options_has_screencast_stream(
        &self,
        id: SessionDesktopOptionsId,
    ) -> Result<bool, PeerError> {
        Ok(self.desktop_options(id)?.screencast_stream()?.is_some())
    }
    /// Return false for a failed native connection, not a guessed fallback.
    pub fn open_desktop_x_display(
        &mut self,
        id: SessionDesktopOptionsId,
        name: &str,
    ) -> Result<bool, PeerError> {
        let display = self.desktop_options(id)?.create_x_display(name)?;
        let Some(display) = display else {
            return Ok(false);
        };
        self.desktop_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .set_x_display(Some(&display))?;
        Ok(true)
    }
    pub fn create_desktop_screencast_stream(
        &mut self,
        id: SessionDesktopOptionsId,
    ) -> Result<(), PeerError> {
        let stream = self.desktop_options(id)?.create_screencast_stream()?;
        self.desktop_options
            .get_mut(&id)
            .ok_or_else(missing)?
            .set_screencast_stream(Some(&stream))
    }
    pub fn copy_desktop_x_display(
        &mut self,
        target: SessionDesktopOptionsId,
        source: Option<SessionDesktopOptionsId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let display = source
            .map(|id| self.desktop_options(id)?.x_display())
            .transpose()?
            .flatten();
        self.desktop_options
            .get_mut(&target)
            .ok_or_else(missing)?
            .set_x_display(display.as_ref())
    }
    pub fn copy_desktop_screencast_stream(
        &mut self,
        target: SessionDesktopOptionsId,
        source: Option<SessionDesktopOptionsId>,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        let stream = source
            .map(|id| self.desktop_options(id)?.screencast_stream())
            .transpose()?
            .flatten();
        self.desktop_options
            .get_mut(&target)
            .ok_or_else(missing)?
            .set_screencast_stream(stream.as_ref())
    }
    pub fn close_desktop_options(&mut self, id: SessionDesktopOptionsId) -> Result<(), PeerError> {
        self.check_open()?;
        self.factory.check_control_call()?;
        self.desktop_options.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn create_desktop_capture(
        &mut self,
        options: SessionDesktopOptionsId,
        kind: crate::DesktopCaptureKind,
        source_id: Option<i64>,
    ) -> Result<SessionSourceId, PeerError> {
        let capture =
            self.factory
                .create_desktop_capture(self.desktop_options(options)?, kind, source_id)?;
        let id = SessionSourceId(next_id()?);
        self.sources.insert(id, Source::Desktop(capture));
        Ok(id)
    }
    pub fn create_desktop_capture_with_callbacks(
        &mut self,
        options: SessionDesktopOptionsId,
        kind: crate::DesktopCaptureKind,
        source_id: Option<i64>,
        receiver: Option<&crate::DesktopCaptureReceiverHandle>,
        observer: Option<&crate::DesktopSelectionObserverHandle>,
    ) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let capture = self.factory.create_desktop_capture_with_callbacks(
            self.desktop_options(options)?,
            kind,
            source_id,
            receiver,
            observer,
        )?;
        let id = SessionSourceId(next_id()?);
        self.sources.insert(id, Source::Desktop(capture));
        Ok(id)
    }
    pub fn create_desktop_capture_with_providers(
        &mut self,
        options: SessionDesktopOptionsId,
        kind: crate::DesktopCaptureKind,
        source_id: Option<i64>,
        receiver: Option<&crate::DesktopCaptureReceiverHandle>,
        observer: Option<&crate::DesktopSelectionObserverHandle>,
        memory_factory: Option<&crate::DesktopMemoryFactoryHandle>,
    ) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let capture = self.factory.create_desktop_capture_with_providers(
            self.desktop_options(options)?,
            kind,
            source_id,
            receiver,
            observer,
            memory_factory,
        )?;
        let id = SessionSourceId(next_id()?);
        self.sources.insert(id, Source::Desktop(capture));
        Ok(id)
    }
    pub fn set_desktop_shared_memory_factory(
        &mut self,
        source: SessionSourceId,
        factory: Option<&crate::DesktopMemoryFactoryHandle>,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?
            .set_shared_memory_factory(factory)
    }
    pub fn capture_desktop_metadata(
        &mut self,
        source: SessionSourceId,
    ) -> Result<Option<SessionDesktopMetadataId>, PeerError> {
        let Some(metadata) = self.desktop_control(source)?.metadata()? else {
            return Ok(None);
        };
        let id = SessionDesktopMetadataId(next_id()?);
        self.desktop_metadata.insert(id, metadata);
        Ok(Some(id))
    }
    fn desktop_metadata(
        &self,
        id: SessionDesktopMetadataId,
    ) -> Result<&crate::DesktopCaptureMetadata, PeerError> {
        self.check_open()?;
        self.desktop_metadata.get(&id).ok_or_else(missing)
    }
    pub fn desktop_metadata_snapshot(
        &self,
        id: SessionDesktopMetadataId,
    ) -> Result<crate::DesktopCaptureMetadataSnapshot, PeerError> {
        Ok(self.desktop_metadata(id)?.snapshot())
    }
    pub fn clone_desktop_metadata(
        &mut self,
        id: SessionDesktopMetadataId,
    ) -> Result<SessionDesktopMetadataId, PeerError> {
        let metadata = self.desktop_metadata(id)?.clone();
        let id = SessionDesktopMetadataId(next_id()?);
        self.desktop_metadata.insert(id, metadata);
        Ok(id)
    }
    pub fn desktop_metadata_same_proxy(
        &self,
        a: SessionDesktopMetadataId,
        b: SessionDesktopMetadataId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .desktop_metadata(a)?
            .same_proxy(self.desktop_metadata(b)?))
    }
    pub fn desktop_metadata_same_cancellable(
        &self,
        a: SessionDesktopMetadataId,
        b: SessionDesktopMetadataId,
    ) -> Result<bool, PeerError> {
        Ok(self
            .desktop_metadata(a)?
            .same_cancellable(self.desktop_metadata(b)?))
    }
    pub fn close_desktop_metadata(
        &mut self,
        id: SessionDesktopMetadataId,
    ) -> Result<(), PeerError> {
        self.check_open()?;
        self.desktop_metadata.remove(&id).ok_or_else(missing)?;
        Ok(())
    }
    pub fn request_desktop_frame(&mut self, source: SessionSourceId) -> Result<(), PeerError> {
        self.desktop_control(source)?.request_frame()
    }
    pub fn set_desktop_receiver(
        &mut self,
        source: SessionSourceId,
        receiver: Option<&crate::DesktopCaptureReceiverHandle>,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?.set_receiver(receiver)
    }
    pub fn set_desktop_selection_observer(
        &mut self,
        source: SessionSourceId,
        observer: Option<&crate::DesktopSelectionObserverHandle>,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?
            .set_selection_observer(observer)
    }
    pub fn camera_devices(&mut self) -> Result<Vec<crate::CameraDevice>, PeerError> {
        self.check_open()?;
        self.factory.camera_devices()
    }
    pub fn camera_formats(
        &mut self,
        device_id: &str,
    ) -> Result<Vec<crate::CameraFormat>, PeerError> {
        self.check_open()?;
        self.factory.camera_formats(device_id)
    }
    pub fn camera_capabilities(
        &mut self,
        device_id: &str,
    ) -> Result<Vec<crate::CameraSettings>, crate::CameraControlError> {
        self.check_open()
            .map_err(crate::CameraControlError::Binding)?;
        self.factory.camera_capabilities(device_id)
    }
    pub fn camera_orientation(
        &mut self,
        device_id: &str,
    ) -> Result<crate::VideoRotation, crate::CameraControlError> {
        self.check_open()
            .map_err(crate::CameraControlError::Binding)?;
        self.factory.camera_orientation(device_id)
    }
    pub fn best_camera_capability(
        &mut self,
        device_id: &str,
        settings: &crate::CameraSettings,
    ) -> Result<(i32, crate::CameraSettings), crate::CameraControlError> {
        self.check_open()
            .map_err(crate::CameraControlError::Binding)?;
        self.factory.best_camera_capability(device_id, settings)
    }
    pub fn screen_sources(&mut self) -> Result<Vec<crate::ScreenSource>, PeerError> {
        self.check_open()?;
        self.factory.screen_sources()
    }
    pub fn window_sources(&mut self) -> Result<Vec<crate::WindowSource>, PeerError> {
        self.check_open()?;
        self.factory.window_sources()
    }
    pub fn set_camera_raw_sink(
        &mut self,
        source: SessionSourceId,
        sink: Option<crate::RawCameraSinkHandle>,
    ) -> Result<(), crate::CameraControlError> {
        self.camera_control(source)?.set_raw_sink(sink)
    }
    fn camera_control(
        &mut self,
        source: SessionSourceId,
    ) -> Result<&mut crate::CameraCapture, crate::CameraControlError> {
        self.check_open()
            .map_err(crate::CameraControlError::Binding)?;
        self.factory
            .check_control_call()
            .map_err(crate::CameraControlError::Binding)?;
        match self.sources.get_mut(&source) {
            Some(Source::Camera(capture)) => Ok(capture),
            _ => Err(crate::CameraControlError::Binding(missing())),
        }
    }
    pub fn camera_settings(
        &mut self,
        source: SessionSourceId,
    ) -> Result<crate::CameraSettings, crate::CameraControlError> {
        self.camera_control(source)?.settings()
    }
    pub fn current_camera_device_name(
        &mut self,
        source: SessionSourceId,
    ) -> Result<String, crate::CameraControlError> {
        self.camera_control(source)?.current_device_name()
    }
    pub fn camera_capture_started(
        &mut self,
        source: SessionSourceId,
    ) -> Result<bool, crate::CameraControlError> {
        self.camera_control(source)?.capture_started()
    }
    pub fn set_camera_rotation(
        &mut self,
        source: SessionSourceId,
        rotation: crate::VideoRotation,
    ) -> Result<(), crate::CameraControlError> {
        self.camera_control(source)?.set_capture_rotation(rotation)
    }
    pub fn set_camera_apply_rotation(
        &mut self,
        source: SessionSourceId,
        enabled: bool,
    ) -> Result<bool, crate::CameraControlError> {
        self.camera_control(source)?.set_apply_rotation(enabled)
    }
    pub fn camera_apply_rotation(
        &mut self,
        source: SessionSourceId,
    ) -> Result<bool, crate::CameraControlError> {
        self.camera_control(source)?.apply_rotation()
    }
    pub fn stop_camera_device(
        &mut self,
        source: SessionSourceId,
    ) -> Result<(), crate::CameraControlError> {
        self.camera_control(source)?.stop_device()
    }
    pub fn start_camera_device(
        &mut self,
        source: SessionSourceId,
        settings: &crate::CameraSettings,
    ) -> Result<(), crate::CameraControlError> {
        self.camera_control(source)?.start_device(settings)
    }
    pub fn create_camera(&mut self, device_id: &str) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let capture = self.factory.create_camera(device_id)?;
        self.sources.insert(id, Source::Camera(capture));
        Ok(id)
    }
    /// Open a native camera in this graph. Publish or replace it using the same
    /// SessionSourceId video operations; no local handle leaves the actor.
    pub fn open_camera(
        &mut self,
        device_id: &str,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let capture = self.factory.open_camera(device_id, width, height, fps)?;
        self.sources.insert(id, Source::Camera(capture));
        Ok(id)
    }
    pub fn open_screen(&mut self, screen_id: i64) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let capture = self.factory.open_screen(screen_id)?;
        self.sources.insert(id, Source::Desktop(capture));
        Ok(id)
    }
    pub fn open_window(&mut self, window_id: i64) -> Result<SessionSourceId, PeerError> {
        self.check_open()?;
        let id = SessionSourceId(next_id()?);
        let capture = self.factory.open_window(window_id)?;
        self.sources.insert(id, Source::Desktop(capture));
        Ok(id)
    }
    fn desktop_control(
        &mut self,
        source: SessionSourceId,
    ) -> Result<&mut crate::ScreenCapture, PeerError> {
        self.check_open()?;
        self.factory.check_control_call()?;
        match self.sources.get_mut(&source) {
            Some(Source::Desktop(capture)) => Ok(capture),
            _ => Err(missing()),
        }
    }
    pub fn desktop_capture_sources(
        &mut self,
        source: SessionSourceId,
    ) -> Result<Vec<crate::ScreenSource>, PeerError> {
        self.desktop_control(source)?.sources()
    }
    pub fn select_desktop_source(
        &mut self,
        source: SessionSourceId,
        id: i64,
    ) -> Result<bool, PeerError> {
        self.desktop_control(source)?.select_source(id)
    }
    pub fn focus_desktop_source(&mut self, source: SessionSourceId) -> Result<bool, PeerError> {
        self.desktop_control(source)?.focus_selected_source()
    }
    pub fn desktop_source_is_occluded(
        &mut self,
        source: SessionSourceId,
        x: i32,
        y: i32,
    ) -> Result<bool, PeerError> {
        self.desktop_control(source)?.is_occluded(x, y)
    }
    pub fn set_desktop_excluded_window(
        &mut self,
        source: SessionSourceId,
        id: i64,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?.set_excluded_window(id)
    }
    pub fn set_desktop_max_frame_rate(
        &mut self,
        source: SessionSourceId,
        rate: u32,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?.set_max_frame_rate(rate)
    }
    pub fn update_desktop_resolution(
        &mut self,
        source: SessionSourceId,
        width: u32,
        height: u32,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?
            .update_resolution(width, height)
    }
    pub fn desktop_has_delegated_selection(
        &mut self,
        source: SessionSourceId,
    ) -> Result<bool, PeerError> {
        self.desktop_control(source)?.has_delegated_selection()
    }
    pub fn ensure_desktop_selection_visible(
        &mut self,
        source: SessionSourceId,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?.ensure_selection_visible()
    }
    pub fn ensure_desktop_selection_hidden(
        &mut self,
        source: SessionSourceId,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?.ensure_selection_hidden()
    }
    pub fn set_desktop_selection_observation(
        &mut self,
        source: SessionSourceId,
        enabled: bool,
    ) -> Result<(), PeerError> {
        self.desktop_control(source)?
            .set_selection_observation(enabled)
    }
    /// Request exactly one native desktop frame. There is no binding timer.
    pub fn capture_next_frame(&mut self, source: SessionSourceId) -> Result<(), PeerError> {
        self.check_open()?;
        self.factory.check_control_call()?;
        match self.sources.get(&source) {
            Some(Source::Desktop(capture)) => capture.capture_next_frame(),
            _ => Err(missing()),
        }
    }
    pub fn screen_capture_status(
        &mut self,
        source: SessionSourceId,
    ) -> Result<crate::ScreenCaptureStatus, PeerError> {
        self.check_open()?;
        match self.sources.get(&source) {
            Some(Source::Desktop(capture)) => Ok(capture.status()),
            _ => Err(missing()),
        }
    }
    pub fn screen_capture_failed_frames(
        &mut self,
        source: SessionSourceId,
    ) -> Result<u64, PeerError> {
        self.check_open()?;
        self.factory.check_control_call()?;
        match self.sources.get(&source) {
            Some(Source::Desktop(capture)) => Ok(capture.failed_frames()),
            _ => Err(missing()),
        }
    }
    pub fn camera_capture_status(
        &mut self,
        source: SessionSourceId,
        stale_after: std::time::Duration,
    ) -> Result<crate::CameraCaptureStatus, PeerError> {
        self.check_open()?;
        self.factory.check_control_call()?;
        match self.sources.get(&source) {
            Some(Source::Camera(capture)) => Ok(capture.status(stale_after)),
            _ => Err(missing()),
        }
    }
}
