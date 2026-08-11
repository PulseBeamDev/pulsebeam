#![allow(clippy::expect_used, clippy::arithmetic_side_effects)] // simulation support
//! Failure injection at points production code already knows can fail.
//!
//! The recovery code beside a fallible call is usually the least tested code in a system: it runs
//! only when something goes wrong, and in a simulator nothing goes wrong unless the simulator
//! makes it. This repo has five route-install failure paths that log an error and bail, and around
//! ninety `debug_assert!`/`fatal!` sites asserting a condition cannot arise. None of that has ever
//! executed under test.
//!
//! FoundationDB's answer, and the one taken here: production declares the point, the simulator
//! decides whether it fires.
//!
//! ```ignore
//! if pulsebeam_runtime::buggify!("data route install") {
//!     return Err(RouteError::Exhausted);
//! }
//! ```
//!
//! Off by default even under `sim`, so the existing suite stays deterministic and green; a plan
//! that wants chaos calls [`enable`]. Without the `sim` feature the call is a `const false` and
//! the branch is compiled out, so production carries nothing.
//!
//! Each site is labelled, and [`fired_sites`] reports which ones actually triggered. A site that
//! never fires across a sweep is a failure path still untested — that list is the point as much as
//! the failures it produces.

#[cfg(feature = "sim")]
mod imp {
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    struct State {
        /// Probability in parts per thousand. Zero means every site is inert.
        permille: u32,
        stream: u64,
        fired: BTreeSet<&'static str>,
        seen: BTreeSet<&'static str>,
    }

    fn state() -> &'static Mutex<State> {
        static STATE: std::sync::OnceLock<Mutex<State>> = std::sync::OnceLock::new();
        STATE.get_or_init(|| {
            Mutex::new(State {
                permille: 0,
                stream: 0,
                fired: BTreeSet::new(),
                seen: BTreeSet::new(),
            })
        })
    }

    /// Arm failure injection for this plan at `permille` parts per thousand, from `seed`.
    ///
    /// Process-global, like the shaper's registries; nextest gives each plan its own process.
    pub fn enable(permille: u32, seed: u64) {
        let mut st = state().lock().expect("buggify state poisoned");
        st.permille = permille;
        st.stream = seed;
        st.fired.clear();
        st.seen.clear();
    }

    /// Sites reached, and of those the ones that fired.
    pub fn coverage() -> (Vec<&'static str>, Vec<&'static str>) {
        let st = state().lock().expect("buggify state poisoned");
        (
            st.seen.iter().copied().collect(),
            st.fired.iter().copied().collect(),
        )
    }

    pub fn fires(site: &'static str) -> bool {
        debug_assert!(
            !site.is_empty(),
            "a buggify site needs a name to be reported"
        );
        let mut st = state().lock().expect("buggify state poisoned");
        st.seen.insert(site);
        if st.permille == 0 {
            return false;
        }
        // SplitMix64, the same stream shape the shaper uses, so a plan seed reproduces the
        // injection sequence exactly.
        st.stream = st.stream.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = st.stream;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let hit = (z % 1000) < u64::from(st.permille);
        if hit {
            st.fired.insert(site);
        }
        hit
    }
}

#[cfg(feature = "sim")]
pub use imp::{coverage, enable, fires};

#[cfg(not(feature = "sim"))]
#[inline(always)]
pub fn fires(_site: &'static str) -> bool {
    false
}

/// Declare a point where something plausible could go wrong.
///
/// See the module docs. Expands to `false` without the `sim` feature.
#[macro_export]
macro_rules! buggify {
    ($site:literal) => {
        $crate::buggify::fires($site)
    };
}
