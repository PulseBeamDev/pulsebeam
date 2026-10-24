//! Owning native statistics. Reports retain the SDK report, including all records.
use crate::{TimeValueError, Timestamp, ffi};
use std::{cell::Cell, collections::BTreeMap, ffi::CStr, fmt, marker::PhantomData};

// Stats values contain only owned strings/containers and native timestamps.
// Reports are immutable const SDK references; mutable builders never alias one.
unsafe impl Send for ffi::NativeRtcStats {}
unsafe impl Sync for ffi::NativeRtcStats {}
unsafe impl Send for ffi::NativeRtcStatsReport {}
unsafe impl Sync for ffi::NativeRtcStatsReport {}
unsafe impl Send for ffi::NativeRtcStatsReportBuilder {}
unsafe impl Send for ffi::NativeRtcStatsAttribute {}
unsafe impl Sync for ffi::NativeRtcStatsAttribute {}

/// The SDK Attribute variant. Undefined retains its native type; integers do
/// not pass through floating-point or JSON conversion.
#[derive(Clone, Debug, PartialEq)]
pub enum RtcStatsValue {
    Bool(Option<bool>),
    Int32(Option<i32>),
    Uint32(Option<u32>),
    Int64(Option<i64>),
    Uint64(Option<u64>),
    Double(Option<f64>),
    String(Option<Vec<u8>>),
    Bools(Option<Vec<bool>>),
    Int32s(Option<Vec<i32>>),
    Uint32s(Option<Vec<u32>>),
    Int64s(Option<Vec<i64>>),
    Uint64s(Option<Vec<u64>>),
    Doubles(Option<Vec<f64>>),
    Strings(Option<Vec<Vec<u8>>>),
    Uint64Map(Option<BTreeMap<Vec<u8>, u64>>),
    DoubleMap(Option<BTreeMap<Vec<u8>, f64>>),
}
impl RtcStatsValue {
    fn native(self) -> ffi::FfiRtcStatsValue {
        match self {
            Self::Bool(value) => <bool as StatField>::pack(value),
            Self::Int32(value) => <i32 as StatField>::pack(value),
            Self::Uint32(value) => <u32 as StatField>::pack(value),
            Self::Int64(value) => <i64 as StatField>::pack(value),
            Self::Uint64(value) => <u64 as StatField>::pack(value),
            Self::Double(value) => <f64 as StatField>::pack(value),
            Self::String(value) => <Vec<u8> as StatField>::pack(value),
            Self::Bools(value) => <Vec<bool> as StatField>::pack(value),
            Self::Int32s(value) => <Vec<i32> as StatField>::pack(value),
            Self::Uint32s(value) => <Vec<u32> as StatField>::pack(value),
            Self::Int64s(value) => <Vec<i64> as StatField>::pack(value),
            Self::Uint64s(value) => <Vec<u64> as StatField>::pack(value),
            Self::Doubles(value) => <Vec<f64> as StatField>::pack(value),
            Self::Strings(value) => <Vec<Vec<u8>> as StatField>::pack(value),
            Self::Uint64Map(value) => <BTreeMap<Vec<u8>, u64> as StatField>::pack(value),
            Self::DoubleMap(value) => <BTreeMap<Vec<u8>, f64> as StatField>::pack(value),
        }
    }
    fn from_native(value: ffi::FfiRtcStatsValue) -> Self {
        match value.kind {
            1 => Self::Bool(<bool as StatField>::unpack(value)),
            2 => Self::Int32(<i32 as StatField>::unpack(value)),
            3 => Self::Uint32(<u32 as StatField>::unpack(value)),
            4 => Self::Int64(<i64 as StatField>::unpack(value)),
            5 => Self::Uint64(<u64 as StatField>::unpack(value)),
            6 => Self::Double(<f64 as StatField>::unpack(value)),
            7 => Self::String(<Vec<u8> as StatField>::unpack(value)),
            8 => Self::Bools(<Vec<bool> as StatField>::unpack(value)),
            9 => Self::Int32s(<Vec<i32> as StatField>::unpack(value)),
            10 => Self::Uint32s(<Vec<u32> as StatField>::unpack(value)),
            11 => Self::Int64s(<Vec<i64> as StatField>::unpack(value)),
            12 => Self::Uint64s(<Vec<u64> as StatField>::unpack(value)),
            13 => Self::Doubles(<Vec<f64> as StatField>::unpack(value)),
            14 => Self::Strings(<Vec<Vec<u8>> as StatField>::unpack(value)),
            15 => Self::Uint64Map(<BTreeMap<Vec<u8>, u64> as StatField>::unpack(value)),
            16 => Self::DoubleMap(<BTreeMap<Vec<u8>, f64> as StatField>::unpack(value)),
            _ => unreachable!("unrecognized pinned stats attribute type"),
        }
    }
}
trait StatField: Sized {
    fn pack(value: Option<Self>) -> ffi::FfiRtcStatsValue;
    fn unpack(value: ffi::FfiRtcStatsValue) -> Option<Self>;
}
macro_rules! stat_field {
    ($ty:ty, $kind:literal, $field:ident, $encode:expr, $decode:expr) => {
        impl StatField for $ty {
            fn pack(value: Option<Self>) -> ffi::FfiRtcStatsValue {
                let mut out = ffi::FfiRtcStatsValue {
                    kind: $kind,
                    ..Default::default()
                };
                if let Some(value) = value {
                    out.has_value = true;
                    out.$field = ($encode)(value);
                }
                out
            }
            fn unpack(value: ffi::FfiRtcStatsValue) -> Option<Self> {
                assert_eq!(value.kind, $kind, "native stats field type mismatch");
                value.has_value.then(|| ($decode)(value.$field))
            }
        }
    };
}
stat_field!(bool, 1, boolean, |v: bool| v, |v: bool| v);
stat_field!(i32, 2, int32_value, |v: i32| v, |v: i32| v);
stat_field!(u32, 3, uint32_value, |v: u32| v, |v: u32| v);
stat_field!(i64, 4, int64_value, |v: i64| v, |v: i64| v);
stat_field!(u64, 5, uint64_value, |v: u64| v, |v: u64| v);
stat_field!(f64, 6, decimal, |v: f64| v, |v: f64| v);
stat_field!(Vec<u8>, 7, text, |v: Vec<u8>| v, |v: Vec<u8>| v);
stat_field!(
    Vec<bool>,
    8,
    booleans,
    |v: Vec<bool>| v.into_iter().map(u8::from).collect(),
    |v: Vec<u8>| v.into_iter().map(|v| v != 0).collect()
);
stat_field!(Vec<i32>, 9, int32s, |v: Vec<i32>| v, |v: Vec<i32>| v);
stat_field!(Vec<u32>, 10, uint32s, |v: Vec<u32>| v, |v: Vec<u32>| v);
stat_field!(Vec<i64>, 11, int64s, |v: Vec<i64>| v, |v: Vec<i64>| v);
stat_field!(Vec<u64>, 12, uint64s, |v: Vec<u64>| v, |v: Vec<u64>| v);
stat_field!(Vec<f64>, 13, decimals, |v: Vec<f64>| v, |v: Vec<f64>| v);
stat_field!(
    Vec<Vec<u8>>,
    14,
    texts,
    |v: Vec<Vec<u8>>| v
        .into_iter()
        .map(|value| ffi::FfiRtcStatsBytes { value })
        .collect(),
    |v: Vec<ffi::FfiRtcStatsBytes>| v.into_iter().map(|v| v.value).collect()
);
stat_field!(
    BTreeMap<Vec<u8>, u64>,
    15,
    uint64_map,
    |v: BTreeMap<Vec<u8>, u64>| v
        .into_iter()
        .map(|(key, value)| ffi::FfiRtcStatsUint64Entry { key, value })
        .collect(),
    |v: Vec<ffi::FfiRtcStatsUint64Entry>| v.into_iter().map(|v| (v.key, v.value)).collect()
);
stat_field!(
    BTreeMap<Vec<u8>, f64>,
    16,
    decimal_map,
    |v: BTreeMap<Vec<u8>, f64>| v
        .into_iter()
        .map(|(key, value)| ffi::FfiRtcStatsDoubleEntry { key, value })
        .collect(),
    |v: Vec<ffi::FfiRtcStatsDoubleEntry>| v.into_iter().map(|v| (v.key, v.value)).collect()
);

macro_rules! stats_objects {
    ($($class:ident = $kind:literal, $values:ident { $($field:ident: $ty:ty,)* })*) => {
        /// SDK class identity, not just its type string. Audio and video source
        /// classes share a type string but are distinct SDK classes.
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
        #[repr(u8)]
        pub enum RtcStatsClass { $($class = $kind,)* }
        impl RtcStatsClass {
            fn from_native(kind: u8) -> Option<Self> { match kind { $($kind => Some(Self::$class),)* _ => None } }
        }
        $(
            #[derive(Clone, Debug, Default, PartialEq)]
            pub struct $values { $(pub $field: Option<$ty>,)* }
        )*
        /// Typed public fields, including inherited and deprecated fields.
        /// None explicitly clears a field; use RtcStats::new for SDK defaults.
        #[derive(Clone, Debug, PartialEq)]
        #[allow(clippy::large_enum_variant, reason = "Preserve the public owning value variants without introducing indirection.")]
        pub enum RtcStatsValues { $($class($values),)* }
        impl RtcStatsValues {
            fn from_native(data: ffi::FfiRtcStatsData) -> Self {
                let mut fields = data.fields.into_iter();
                let value = match data.class_kind {
                    $($kind => Self::$class($values { $($field: <$ty as StatField>::unpack(fields.next().expect("native stats field")),)* }),)*
                    _ => unreachable!("unrecognized pinned stats class"),
                };
                assert!(fields.next().is_none(), "extra native stats fields"); value
            }
            fn native(self) -> ffi::FfiRtcStatsData { match self {
                $(Self::$class(value) => ffi::FfiRtcStatsData { class_kind: $kind, fields: vec![$(<$ty as StatField>::pack(value.$field),)*] },)*
            }}
        }
    }
}
include!("stats_values.rs");

/// An owning copy of a typed optional native Attribute. A CStr name reflects
/// the SDK's NUL-terminated name contract; string values have no such restriction.
pub struct RtcStatsAttribute(cxx::UniquePtr<ffi::NativeRtcStatsAttribute>);
impl RtcStatsAttribute {
    pub fn new(name: &CStr, value: RtcStatsValue) -> Self {
        Self(ffi::rtc_stats_attribute_new(
            name.to_bytes(),
            &value.native(),
        ))
    }
    fn native(&self) -> &ffi::NativeRtcStatsAttribute {
        self.0.as_ref().unwrap()
    }
    pub fn name(&self) -> Vec<u8> {
        ffi::rtc_stats_attribute_data(self.native()).name
    }
    pub fn value(&self) -> RtcStatsValue {
        RtcStatsValue::from_native(ffi::rtc_stats_attribute_data(self.native()).value)
    }
    pub fn has_value(&self) -> bool {
        ffi::rtc_stats_attribute_has_value(self.native())
    }
    pub fn is_sequence(&self) -> bool {
        ffi::rtc_stats_attribute_is_sequence(self.native())
    }
    pub fn is_string(&self) -> bool {
        ffi::rtc_stats_attribute_is_string(self.native())
    }
    /// Exact native Attribute::ToString output, not a lossless numeric serializer.
    pub fn to_native_string(&self) -> Vec<u8> {
        ffi::rtc_stats_attribute_string(self.native())
    }
}
impl Clone for RtcStatsAttribute {
    fn clone(&self) -> Self {
        Self(ffi::rtc_stats_attribute_copy(self.native()))
    }
}
impl PartialEq for RtcStatsAttribute {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtc_stats_attribute_equal(self.native(), other.native())
    }
}
impl fmt::Debug for RtcStatsAttribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtcStatsAttribute")
            .field("name", &self.name())
            .field("value", &self.value())
            .finish()
    }
}

/// Owned SDK RTCStats value. Copies invoke the actual virtual SDK copy method.
pub struct RtcStats(cxx::UniquePtr<ffi::NativeRtcStats>);
impl RtcStats {
    /// None denotes one of the five SDK base classes with protected constructors.
    /// Their public copy constructors remain available with copy_as.
    pub fn new(class: RtcStatsClass, id: &[u8], timestamp: Timestamp) -> Option<Self> {
        Self::from_native(ffi::rtc_stats_new(
            class as u8,
            id,
            timestamp.native_microsecond_bits(),
        ))
    }
    fn from_native(value: cxx::UniquePtr<ffi::NativeRtcStats>) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }
    fn native(&self) -> &ffi::NativeRtcStats {
        self.0.as_ref().unwrap()
    }
    pub fn class(&self) -> RtcStatsClass {
        RtcStatsClass::from_native(ffi::rtc_stats_class(self.native())).expect("pinned stats class")
    }
    pub fn id(&self) -> Vec<u8> {
        ffi::rtc_stats_id(self.native())
    }
    pub fn timestamp(&self) -> Timestamp {
        Timestamp::from_native_microsecond_bits(ffi::rtc_stats_timestamp(self.native()))
    }
    pub fn set_timestamp(&mut self, value: Timestamp) {
        ffi::rtc_stats_set_timestamp(self.0.pin_mut(), value.native_microsecond_bits());
    }
    pub fn native_type(&self) -> Vec<u8> {
        ffi::rtc_stats_type(self.native())
    }
    pub fn values(&self) -> RtcStatsValues {
        RtcStatsValues::from_native(ffi::rtc_stats_data(self.native()))
    }
    /// False leaves the object unchanged when the values belong to another class.
    pub fn set_values(&mut self, values: RtcStatsValues) -> bool {
        ffi::rtc_stats_set_data(self.0.pin_mut(), &values.native())
    }
    /// Native copy construction as this class or a true inherited base.
    pub fn copy_as(&self, class: RtcStatsClass) -> Option<Self> {
        Self::from_native(ffi::rtc_stats_copy_as(self.native(), class as u8))
    }
    /// Actual SDK attribute ordering, names, type and optional presence.
    pub fn attributes(&self) -> Vec<RtcStatsAttribute> {
        ffi::rtc_stats_attributes(self.native())
            .into_iter()
            .map(|a| {
                RtcStatsAttribute::new(
                    CStr::from_bytes_with_nul(&[a.name.as_slice(), &[0]].concat())
                        .expect("SDK name"),
                    RtcStatsValue::from_native(a.value),
                )
            })
            .collect()
    }
    /// Exact SDK serialization, including upstream quoting/precision limitations.
    /// Native JSON serialization requires finite object timestamps.
    pub fn to_native_json(&self) -> Result<Vec<u8>, TimeValueError> {
        if !self.timestamp().is_finite() {
            return Err(TimeValueError::InfiniteValue);
        }
        Ok(ffi::rtc_stats_json(self.native()))
    }
}
impl Clone for RtcStats {
    fn clone(&self) -> Self {
        Self(ffi::rtc_stats_copy(self.native()))
    }
}
/// SDK equality intentionally ignores timestamps.
impl PartialEq for RtcStats {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtc_stats_equal(self.native(), other.native())
    }
}
impl fmt::Debug for RtcStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtcStats")
            .field("id", &self.id())
            .field("timestamp", &self.timestamp())
            .field("values", &self.values())
            .finish()
    }
}

/// A retained immutable SDK report, independent of peer/factory lifetime.
/// Clones share the actual report; copy_builder performs the SDK deep copy.
pub struct RtcStatsReport(cxx::UniquePtr<ffi::NativeRtcStatsReport>);
impl RtcStatsReport {
    pub(crate) fn from_native(value: cxx::UniquePtr<ffi::NativeRtcStatsReport>) -> Self {
        assert!(!value.is_null());
        Self(value)
    }
    fn native(&self) -> &ffi::NativeRtcStatsReport {
        self.0.as_ref().unwrap()
    }
    pub fn timestamp(&self) -> Timestamp {
        Timestamp::from_native_microsecond_bits(ffi::rtc_stats_report_timestamp(self.native()))
    }
    pub fn len(&self) -> usize {
        ffi::rtc_stats_report_size(self.native())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::rtc_stats_report_same(self.native(), other.native())
    }
    pub fn get(&self, id: &[u8]) -> Option<RtcStats> {
        RtcStats::from_native(ffi::rtc_stats_report_get(self.native(), id, 0))
    }
    pub fn get_as(&self, id: &[u8], class: RtcStatsClass) -> Option<RtcStats> {
        RtcStats::from_native(ffi::rtc_stats_report_get(self.native(), id, class as u8))
    }
    pub fn iter(&self) -> impl Iterator<Item = RtcStats> + '_ {
        ffi::rtc_stats_report_keys(self.native())
            .into_iter()
            .map(|id| self.get(&id.value).expect("immutable report member"))
    }
    pub fn of_class(&self, class: RtcStatsClass) -> Vec<RtcStats> {
        ffi::rtc_stats_report_of_class(self.native(), class as u8)
            .into_iter()
            .map(|v| RtcStats::from_native(v.value).unwrap())
            .collect()
    }
    pub fn copy_builder(&self) -> RtcStatsReportBuilder {
        RtcStatsReportBuilder(ffi::rtc_stats_report_copy(self.native()), PhantomData)
    }
    pub fn to_native_json(&self) -> Result<Vec<u8>, TimeValueError> {
        if !ffi::rtc_stats_report_serializable(self.native()) {
            return Err(TimeValueError::InfiniteValue);
        }
        Ok(ffi::rtc_stats_report_json(self.native()))
    }
}
/// Structural report equality includes ids and timestamps; RTCStats value
/// equality itself is the SDK's timestamp-independent comparison.
impl PartialEq for RtcStatsReport {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self.timestamp() == other.timestamp()
            && self
                .iter()
                .zip(other.iter())
                .all(|(a, b)| a.id() == b.id() && a.timestamp() == b.timestamp() && a == b)
    }
}
impl Clone for RtcStatsReport {
    fn clone(&self) -> Self {
        Self(ffi::rtc_stats_report_clone(self.native()))
    }
}
impl fmt::Debug for RtcStatsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RtcStatsReport")
            .field("timestamp", &self.timestamp())
            .field("len", &self.len())
            .finish()
    }
}

/// Single-owner native report mutation. Freezing consumes the only mutable owner.
pub struct RtcStatsReportBuilder(
    cxx::UniquePtr<ffi::NativeRtcStatsReportBuilder>,
    PhantomData<Cell<()>>,
);
impl RtcStatsReportBuilder {
    pub fn new(timestamp: Timestamp) -> Self {
        Self(
            ffi::rtc_stats_report_new(timestamp.native_microsecond_bits()),
            PhantomData,
        )
    }
    pub fn len(&self) -> usize {
        ffi::rtc_stats_builder_size(self.0.as_ref().unwrap())
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// SDK AddStats with a duplicate-id precondition guard; the consumed value
    /// is destroyed on rejection, just as with SDK TryAddStats.
    pub fn add(&mut self, value: RtcStats) -> bool {
        ffi::rtc_stats_builder_add(self.0.pin_mut(), value.0, false)
    }
    pub fn try_add(&mut self, value: RtcStats) -> bool {
        ffi::rtc_stats_builder_add(self.0.pin_mut(), value.0, true)
    }
    pub fn take(&mut self, id: &[u8]) -> Option<RtcStats> {
        RtcStats::from_native(ffi::rtc_stats_builder_take(self.0.pin_mut(), id))
    }
    /// False leaves both reports unchanged if ids overlap, protecting the SDK
    /// duplicate-insertion precondition. Otherwise invokes TakeMembersFrom.
    pub fn take_members_from(&mut self, other: &mut Self) -> bool {
        ffi::rtc_stats_builder_merge(self.0.pin_mut(), other.0.pin_mut())
    }
    pub fn freeze(self) -> RtcStatsReport {
        RtcStatsReport::from_native(ffi::rtc_stats_report_freeze(self.0))
    }
}
