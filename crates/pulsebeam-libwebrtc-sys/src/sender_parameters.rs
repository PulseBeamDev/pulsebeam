//! Exact full-value sender transactions, including native asynchronous completion.
use crate::{PeerError, PeerErrorKind, RtcError, RtpParameters, RtpSender, ffi};

/// Clones observe one actual completion and share its one-shot result.
/// Dropping a handle does not cancel or invent a result for the SDK operation.
/// This pure observation resource contains no callable sender/queue handle.
#[derive(Clone)]
pub struct SenderParametersOperation {
    native: cxx::SharedPtr<ffi::NativeSetParametersOperation>,
}
// SAFETY: native state contains only a mutex-protected owned RTCError.
unsafe impl Send for SenderParametersOperation {}
unsafe impl Sync for SenderParametersOperation {}
impl SenderParametersOperation {
    pub fn is_ready(&self) -> bool {
        ffi::set_parameters_operation_ready(
            self.native.as_ref().expect("native parameter operation"),
        )
    }
    pub fn take_result(&self) -> Option<RtcError> {
        let value = ffi::set_parameters_operation_take(
            self.native.as_ref().expect("native parameter operation"),
        );
        (!value.is_null()).then(|| RtcError::from_native(value))
    }
}
impl RtpSender {
    fn admit_parameter_value(&self, parameters: &RtpParameters) -> Result<(), PeerError> {
        let fail = |kind, message: &str| PeerError {
            kind,
            message: message.into(),
        };
        if self.peer.closed.get() {
            return Err(fail(PeerErrorKind::Closed, "peer is closed"));
        }
        if let Some(track) = self.track().filter(|track| track.is_direct_encoded()) {
            let encodings = parameters.encodings();
            let simulcast = track.is_direct_simulcast();
            let temporal = track.is_direct_l1t3();
            if (simulcast && encodings.len() != 3) || (!simulcast && encodings.len() > 1) {
                return Err(fail(
                    PeerErrorKind::UnsupportedParameter,
                    "direct encoded source profile does not support this encoding count",
                ));
            }
            for encoding in encodings {
                let mode = encoding.scalability_mode_bytes();
                if mode
                    .as_deref()
                    .is_some_and(|mode| !(temporal && mode == b"L1T3"))
                    || (!simulcast
                        && (encoding.scale_resolution_down_to().is_some()
                            || encoding
                                .scale_resolution_down_by()
                                .is_some_and(|scale| scale != 1.0)))
                    || encoding
                        .num_temporal_layers()
                        .is_some_and(|layers| layers != if temporal { 3 } else { 1 })
                {
                    return Err(fail(
                        PeerErrorKind::UnsupportedParameter,
                        "parameters are unsupported by the direct encoded source profile",
                    ));
                }
                if mode.as_deref() == Some(b"L1T3") && !self.has_negotiated_dependency_descriptor()
                {
                    return Err(fail(
                        PeerErrorKind::UnsupportedParameter,
                        "direct L1T3 requires negotiated native dependency descriptors",
                    ));
                }
            }
        }
        Ok(())
    }
    /// Invoke SDK SetParameters once with the entire supplied transaction.
    /// No implicit Get, partial overlay, guessed rollback or status conversion.
    pub fn set_parameters_value(&self, parameters: &RtpParameters) -> Result<RtcError, PeerError> {
        self.admit_parameter_value(parameters)?;
        let mut valid = false;
        let result =
            ffi::rtp_sender_set_parameters_value(self.native(), parameters.native(), &mut valid);
        if !valid {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "unsafe native sender parameter arithmetic".into(),
            });
        }
        Ok(RtcError::from_native(result))
    }
    /// Dispatch SDK SetParametersAsync. Completion may be immediate; a launch
    /// is not an acknowledgment and does not promise atomic reconfiguration.
    pub fn set_parameters_async(
        &self,
        parameters: &RtpParameters,
    ) -> Result<SenderParametersOperation, PeerError> {
        self.admit_parameter_value(parameters)?;
        let mut valid = false;
        let native =
            ffi::rtp_sender_set_parameters_async(self.native(), parameters.native(), &mut valid);
        if !valid {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "unsafe native sender parameter arithmetic".into(),
            });
        }
        Ok(SenderParametersOperation { native })
    }
}
