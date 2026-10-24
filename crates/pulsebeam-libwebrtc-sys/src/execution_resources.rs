//! Native execution-resource defaults, independent of media implementation selection.

/// A positive native `int` codec-core budget. This is an execution hint, not a
/// process-wide worker ceiling. Individual native backends may use fewer cores
/// or manage workers independently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecCoreBudget(i32);

impl CodecCoreBudget {
    pub const fn new(cores: i32) -> Result<Self, InvalidCodecCoreBudget> {
        if cores > 0 {
            Ok(Self(cores))
        } else {
            Err(InvalidCodecCoreBudget)
        }
    }

    pub const fn cores(self) -> i32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCodecCoreBudget;

impl std::fmt::Display for InvalidCodecCoreBudget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("codec-core budget must be a positive native int")
    }
}
impl std::error::Error for InvalidCodecCoreBudget {}

/// Native Call execution-resource configuration, independent of media selection.
/// When no configuration is registered, a caller-driven peer factory supplies
/// a one-core default; a threaded factory preserves native host detection.
/// Explicit configurations and standalone codec settings are not rewritten.
/// A budget is not a worker ceiling or deterministic-backend qualification.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutionResources {
    /// Within an explicitly registered configuration, unset preserves native
    /// host core detection. A selected budget reaches native video send and
    /// receive codec settings. An explicit native `CallConfig` budget takes
    /// precedence over this factory default.
    pub codec_core_budget: Option<CodecCoreBudget>,
}

impl ExecutionResources {
    pub(crate) fn resolve(registration: Option<Self>, caller_driven: bool) -> Self {
        registration.unwrap_or(Self {
            codec_core_budget: caller_driven.then_some(CodecCoreBudget(1)),
        })
    }

    pub(crate) fn codec_core_budget_bits(self) -> i32 {
        self.codec_core_budget.map_or(0, CodecCoreBudget::cores)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_default_does_not_rewrite_explicit_resource_configuration() {
        assert_eq!(
            ExecutionResources::resolve(None, false).codec_core_budget_bits(),
            0
        );
        assert_eq!(
            ExecutionResources::resolve(None, true).codec_core_budget_bits(),
            1
        );
        for caller_driven in [false, true] {
            for cores in [None, Some(1), Some(2), Some(i32::MAX)] {
                let configured = ExecutionResources {
                    codec_core_budget: cores.map(|cores| CodecCoreBudget::new(cores).unwrap()),
                };
                assert_eq!(
                    ExecutionResources::resolve(Some(configured), caller_driven),
                    configured
                );
            }
        }
    }

    #[test]
    fn full_positive_native_domain_and_absent_default_are_preserved() {
        assert_eq!(ExecutionResources::default().codec_core_budget_bits(), 0);
        for cores in [1, 2, i32::MAX] {
            let budget = CodecCoreBudget::new(cores).unwrap();
            assert_eq!(budget.cores(), cores);
            assert_eq!(
                ExecutionResources {
                    codec_core_budget: Some(budget)
                }
                .codec_core_budget_bits(),
                cores
            );
        }
        for cores in [i32::MIN, -1, 0] {
            assert_eq!(CodecCoreBudget::new(cores), Err(InvalidCodecCoreBudget));
        }
    }
}
