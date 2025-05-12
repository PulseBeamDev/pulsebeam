use common::setup_sim;
mod common;

#[test]
fn basic() {
    let mut sim = setup_sim(1);
    sim.run().unwrap();
}
