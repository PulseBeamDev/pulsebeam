pub mod pulsebeam {
    pub mod v1 {
        tonic::include_proto!("pulsebeam.v1");
    }
}
pub use pulsebeam::v1::*;
