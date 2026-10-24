use crate::{VideoMetadataError, ffi};

/// Native H.264 packetization metadata. The non-fixed C++ enum admits 0..=3;
/// value 3 is unnamed. Packetizer admission is a separate consumption contract.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H264PacketizationType(pub i32);
impl H264PacketizationType {
    pub const SINGLE_NALU: Self = Self(0);
    pub const STAP_A: Self = Self(1);
    pub const FU_A: Self = Self(2);
}

/// Full signed-int domain of the SDK's scoped packetization-mode enum.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H264PacketizationMode(pub i32);
impl H264PacketizationMode {
    pub const NON_INTERLEAVED: Self = Self(0);
    pub const SINGLE_NAL_UNIT: Self = Self(1);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct H264NaluInfo {
    pub type_code: u8,
    pub sps_id: i32,
    pub pps_id: i32,
}

/// Full physical aggregate storage. Rust PartialEq compares every field;
/// native SDK equality and its prefix/flag rules are separate operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpVideoHeaderVp8 {
    pub non_reference: bool,
    pub picture_id: i16,
    pub tl0_pic_idx: i16,
    pub temporal_idx: u8,
    pub layer_sync: bool,
    pub key_idx: i32,
    pub partition_id: i32,
    pub beginning_of_partition: bool,
}
impl Default for RtpVideoHeaderVp8 {
    fn default() -> Self {
        Self::from_native(ffi::video_header_vp8_defaults())
    }
}
impl RtpVideoHeaderVp8 {
    pub fn init(&mut self) {
        let mut value = self.to_native();
        ffi::video_header_vp8_init(&mut value);
        *self = Self::from_native(value);
    }
    pub fn sdk_equals(&self, other: &Self) -> bool {
        match ffi::video_header_vp8_equals(self.to_native(), other.to_native()) {
            0 => false,
            1 => true,
            _ => unreachable!("VP8 equality has no counted arrays"),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiVideoHeaderVp8) -> Self {
        Self {
            non_reference: value.non_reference,
            picture_id: value.picture_id,
            tl0_pic_idx: value.tl0_pic_idx,
            temporal_idx: value.temporal_idx,
            layer_sync: value.layer_sync,
            key_idx: value.key_idx,
            partition_id: value.partition_id,
            beginning_of_partition: value.beginning_of_partition,
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiVideoHeaderVp8 {
        ffi::FfiVideoHeaderVp8 {
            non_reference: self.non_reference,
            picture_id: self.picture_id,
            tl0_pic_idx: self.tl0_pic_idx,
            temporal_idx: self.temporal_idx,
            layer_sync: self.layer_sync,
            key_idx: self.key_idx,
            partition_id: self.partition_id,
            beginning_of_partition: self.beginning_of_partition,
        }
    }
}

/// Native temporal structure selection. Values 0..=3 are representable; 3
/// reaches SDK default/not-reached behavior, preserved for assertion-free builds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Vp9TemporalStructureMode(pub i32);
impl Vp9TemporalStructureMode {
    pub const ONE_LAYER: Self = Self(0);
    pub const TWO_LAYERS: Self = Self(1);
    pub const THREE_LAYERS: Self = Self(2);
}

pub(crate) fn operation_status(status: i32) -> Result<(), VideoMetadataError> {
    let error = match status {
        0 => return Ok(()),
        4 => VideoMetadataError::NativeEnumRange("H264 packetization type"),
        6 => VideoMetadataError::NativeConsumption("GOF frame count exceeds physical 255 rows"),
        7 => VideoMetadataError::NativeConsumption("GOF reference count exceeds physical 3 slots"),
        8 => VideoMetadataError::NativeAssertion("GofInfoVP9::SetGofInfoVP9"),
        9 => VideoMetadataError::NativeConsumption("VP9 reference count exceeds physical 3 slots"),
        10 => VideoMetadataError::NativeConsumption("VP9 spatial count exceeds physical 8 layers"),
        11 => VideoMetadataError::NativeEnumRange("VP9 temporal structure mode"),
        _ => unreachable!("binding-generated variant and physical array shape"),
    };
    Err(error)
}
pub(crate) fn equality_status(status: i32) -> Result<bool, VideoMetadataError> {
    match status {
        0 => Ok(false),
        1 => Ok(true),
        error => {
            operation_status(error)?;
            unreachable!("non-boolean SDK equality result")
        }
    }
}

/// Full GOF physical storage, including inactive rows and pid_start.
/// Metadata counts are retained even if unsafe for a later native consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Vp9GofInfo {
    pub num_frames_in_gof: usize,
    pub temporal_idx: [u8; 255],
    pub temporal_up_switch: [bool; 255],
    pub num_ref_pics: [u8; 255],
    pub pid_diff: [[u8; 3]; 255],
    pub pid_start: u16,
}
impl Default for Vp9GofInfo {
    fn default() -> Self {
        RtpVideoHeaderVp9::default().gof
    }
}
impl Vp9GofInfo {
    /// Native structure setup updates only its selected prefix, retaining
    /// pid_start, unused rows and unused reference slots.
    pub fn set_structure(
        &mut self,
        mode: Vp9TemporalStructureMode,
    ) -> Result<(), VideoMetadataError> {
        let mut value = self.to_native();
        operation_status(ffi::vp9_gof_set_structure(&mut value, mode.0))?;
        *self = Self::from_native(value);
        Ok(())
    }
    /// Native CopyGofInfoVP9 copies its active prefix, not pid_start or tails.
    /// Source counts are checked only for the arrays this operation consumes.
    /// An unsafe source leaves the target unchanged.
    pub fn copy_prefix_from(&mut self, source: &Self) -> Result<(), VideoMetadataError> {
        let mut value = self.to_native();
        operation_status(ffi::vp9_gof_copy_prefix(&mut value, source.to_native()))?;
        *self = Self::from_native(value);
        Ok(())
    }
    pub fn sdk_equals(&self, other: &Self) -> Result<bool, VideoMetadataError> {
        equality_status(ffi::vp9_gof_equals(self.to_native(), other.to_native()))
    }
    pub(crate) fn from_native(value: ffi::FfiVp9GofInfo) -> Self {
        let flat: [u8; 765] = value.pid_diff.try_into().expect("native GOF physical size");
        Self {
            num_frames_in_gof: value.num_frames_in_gof,
            temporal_idx: value
                .temporal_idx
                .try_into()
                .expect("native GOF physical size"),
            temporal_up_switch: value
                .temporal_up_switch
                .try_into()
                .expect("native GOF physical size"),
            num_ref_pics: value
                .num_ref_pics
                .try_into()
                .expect("native GOF physical size"),
            pid_diff: std::array::from_fn(|row| std::array::from_fn(|r| flat[row * 3 + r])),
            pid_start: value.pid_start,
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiVp9GofInfo {
        ffi::FfiVp9GofInfo {
            num_frames_in_gof: self.num_frames_in_gof,
            temporal_idx: self.temporal_idx.to_vec(),
            temporal_up_switch: self.temporal_up_switch.to_vec(),
            num_ref_pics: self.num_ref_pics.to_vec(),
            pid_diff: self.pid_diff.iter().flatten().copied().collect(),
            pid_start: self.pid_start,
        }
    }
}

/// Complete VP9 header storage, not a wire descriptor. Default value-initializes
/// the aggregate then delegates to SDK InitRTPVideoHeaderVP9. This initializes
/// untouched array tails/GOF instead of reproducing raw C++ indeterminate storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpVideoHeaderVp9 {
    pub inter_pic_predicted: bool,
    pub flexible_mode: bool,
    pub beginning_of_frame: bool,
    pub end_of_frame: bool,
    pub ss_data_available: bool,
    pub non_ref_for_inter_layer_pred: bool,
    pub picture_id: i16,
    pub max_picture_id: i16,
    pub tl0_pic_idx: i16,
    pub temporal_idx: u8,
    pub spatial_idx: u8,
    pub temporal_up_switch: bool,
    pub inter_layer_predicted: bool,
    pub gof_idx: u8,
    pub num_ref_pics: u8,
    pub pid_diff: [u8; 3],
    pub ref_picture_id: [i16; 3],
    pub num_spatial_layers: usize,
    pub first_active_layer: usize,
    pub spatial_layer_resolution_present: bool,
    pub width: [u16; 8],
    pub height: [u16; 8],
    pub gof: Vp9GofInfo,
    pub end_of_picture: bool,
}
impl Default for RtpVideoHeaderVp9 {
    fn default() -> Self {
        Self::from_native(ffi::video_header_vp9_defaults())
    }
}
impl RtpVideoHeaderVp9 {
    /// Delegates to SDK Init, preserving untouched arrays, the resolution
    /// flag and GOF. This does not replace the aggregate with Default.
    pub fn init(&mut self) {
        let mut value = self.to_native();
        operation_status(ffi::video_header_vp9_init(&mut value))
            .expect("binding-generated physical array shape");
        *self = Self::from_native(value);
    }
    pub fn sdk_equals(&self, other: &Self) -> Result<bool, VideoMetadataError> {
        equality_status(ffi::video_header_vp9_equals(
            self.to_native(),
            other.to_native(),
        ))
    }
    pub(crate) fn from_native(value: ffi::FfiVideoHeaderVp9) -> Self {
        Self {
            inter_pic_predicted: value.inter_pic_predicted,
            flexible_mode: value.flexible_mode,
            beginning_of_frame: value.beginning_of_frame,
            end_of_frame: value.end_of_frame,
            ss_data_available: value.ss_data_available,
            non_ref_for_inter_layer_pred: value.non_ref_for_inter_layer_pred,
            picture_id: value.picture_id,
            max_picture_id: value.max_picture_id,
            tl0_pic_idx: value.tl0_pic_idx,
            temporal_idx: value.temporal_idx,
            spatial_idx: value.spatial_idx,
            temporal_up_switch: value.temporal_up_switch,
            inter_layer_predicted: value.inter_layer_predicted,
            gof_idx: value.gof_idx,
            num_ref_pics: value.num_ref_pics,
            pid_diff: value.pid_diff.try_into().expect("native VP9 physical size"),
            ref_picture_id: value
                .ref_picture_id
                .try_into()
                .expect("native VP9 physical size"),
            num_spatial_layers: value.num_spatial_layers,
            first_active_layer: value.first_active_layer,
            spatial_layer_resolution_present: value.spatial_layer_resolution_present,
            width: value.width.try_into().expect("native VP9 physical size"),
            height: value.height.try_into().expect("native VP9 physical size"),
            gof: Vp9GofInfo::from_native(value.gof),
            end_of_picture: value.end_of_picture,
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiVideoHeaderVp9 {
        ffi::FfiVideoHeaderVp9 {
            inter_pic_predicted: self.inter_pic_predicted,
            flexible_mode: self.flexible_mode,
            beginning_of_frame: self.beginning_of_frame,
            end_of_frame: self.end_of_frame,
            ss_data_available: self.ss_data_available,
            non_ref_for_inter_layer_pred: self.non_ref_for_inter_layer_pred,
            picture_id: self.picture_id,
            max_picture_id: self.max_picture_id,
            tl0_pic_idx: self.tl0_pic_idx,
            temporal_idx: self.temporal_idx,
            spatial_idx: self.spatial_idx,
            temporal_up_switch: self.temporal_up_switch,
            inter_layer_predicted: self.inter_layer_predicted,
            gof_idx: self.gof_idx,
            num_ref_pics: self.num_ref_pics,
            pid_diff: self.pid_diff.to_vec(),
            ref_picture_id: self.ref_picture_id.to_vec(),
            num_spatial_layers: self.num_spatial_layers,
            first_active_layer: self.first_active_layer,
            spatial_layer_resolution_present: self.spatial_layer_resolution_present,
            width: self.width.to_vec(),
            height: self.height.to_vec(),
            gof: self.gof.to_native(),
            end_of_picture: self.end_of_picture,
        }
    }
}

/// Complete H.264 metadata, with an ordered, owned NAL list and open enum tokens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpVideoHeaderH264 {
    pub nalu_type: u8,
    pub packetization_type: H264PacketizationType,
    pub nalus: Vec<H264NaluInfo>,
    pub packetization_mode: H264PacketizationMode,
}
impl Default for RtpVideoHeaderH264 {
    fn default() -> Self {
        Self::from_native(ffi::video_header_h264_defaults())
    }
}
impl RtpVideoHeaderH264 {
    pub fn sdk_equals(&self, other: &Self) -> Result<bool, VideoMetadataError> {
        equality_status(ffi::video_header_h264_equals(
            self.to_native(),
            other.to_native(),
        ))
    }
    pub(crate) fn from_native(value: ffi::FfiVideoHeaderH264) -> Self {
        Self {
            nalu_type: value.nalu_type,
            packetization_type: H264PacketizationType(value.packetization_type),
            nalus: value
                .nalus
                .into_iter()
                .map(|nalu| H264NaluInfo {
                    type_code: nalu.type_code,
                    sps_id: nalu.sps_id,
                    pps_id: nalu.pps_id,
                })
                .collect(),
            packetization_mode: H264PacketizationMode(value.packetization_mode),
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiVideoHeaderH264 {
        ffi::FfiVideoHeaderH264 {
            nalu_type: self.nalu_type,
            packetization_type: self.packetization_type.0,
            nalus: self
                .nalus
                .iter()
                .map(|nalu| ffi::FfiH264NaluInfo {
                    type_code: nalu.type_code,
                    sps_id: nalu.sps_id,
                    pps_id: nalu.pps_id,
                })
                .collect(),
            packetization_mode: self.packetization_mode.0,
        }
    }
}

/// Independent copy of the SDK variant's complete storage. Variant and codec
/// metadata may disagree, as permitted by the native metadata setters. Header
/// transport admission must account for the particular native consumer.
/// Rust PartialEq compares full storage, not the SDK's conditional equality.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RtpVideoHeaderCodecSpecifics {
    #[default]
    None,
    Vp8(RtpVideoHeaderVp8),
    Vp9(Box<RtpVideoHeaderVp9>),
    H264(RtpVideoHeaderH264),
}
impl RtpVideoHeaderCodecSpecifics {
    pub(crate) fn from_native(value: ffi::FfiVideoHeaderCodecSpecifics) -> Self {
        match value.tag {
            0 => Self::None,
            1 => Self::Vp8(RtpVideoHeaderVp8::from_native(value.vp8)),
            2 => Self::Vp9(Box::new(RtpVideoHeaderVp9::from_native(value.vp9))),
            3 => Self::H264(RtpVideoHeaderH264::from_native(value.h264)),
            _ => unreachable!("SDK metadata variant tag"),
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiVideoHeaderCodecSpecifics {
        let mut value = empty_specifics();
        match self {
            Self::None => {}
            Self::Vp8(vp8) => {
                value.tag = 1;
                value.vp8 = vp8.to_native();
            }
            Self::Vp9(vp9) => {
                value.tag = 2;
                value.vp9 = vp9.to_native();
            }
            Self::H264(h264) => {
                value.tag = 3;
                value.h264 = h264.to_native();
            }
        }
        value
    }
    pub(crate) fn check_set_status(status: i32) -> Result<(), VideoMetadataError> {
        match status {
            0 => Ok(()),
            4 => Err(VideoMetadataError::NativeEnumRange(
                "H264 packetization type",
            )),
            _ => unreachable!("binding-generated variant and physical array shape"),
        }
    }
}
fn empty_specifics() -> ffi::FfiVideoHeaderCodecSpecifics {
    ffi::FfiVideoHeaderCodecSpecifics {
        tag: 0,
        vp8: ffi::FfiVideoHeaderVp8 {
            non_reference: false,
            picture_id: 0,
            tl0_pic_idx: 0,
            temporal_idx: 0,
            layer_sync: false,
            key_idx: 0,
            partition_id: 0,
            beginning_of_partition: false,
        },
        vp9: ffi::FfiVideoHeaderVp9 {
            inter_pic_predicted: false,
            flexible_mode: false,
            beginning_of_frame: false,
            end_of_frame: false,
            ss_data_available: false,
            non_ref_for_inter_layer_pred: false,
            picture_id: 0,
            max_picture_id: 0,
            tl0_pic_idx: 0,
            temporal_idx: 0,
            spatial_idx: 0,
            temporal_up_switch: false,
            inter_layer_predicted: false,
            gof_idx: 0,
            num_ref_pics: 0,
            pid_diff: Vec::new(),
            ref_picture_id: Vec::new(),
            num_spatial_layers: 0,
            first_active_layer: 0,
            spatial_layer_resolution_present: false,
            width: Vec::new(),
            height: Vec::new(),
            gof: ffi::FfiVp9GofInfo {
                num_frames_in_gof: 0,
                temporal_idx: Vec::new(),
                temporal_up_switch: Vec::new(),
                num_ref_pics: Vec::new(),
                pid_diff: Vec::new(),
                pid_start: 0,
            },
            end_of_picture: false,
        },
        h264: ffi::FfiVideoHeaderH264 {
            nalu_type: 0,
            packetization_type: 0,
            nalus: Vec::new(),
            packetization_mode: 0,
        },
    }
}
