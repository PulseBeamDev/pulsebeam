use common::setup_sim;
mod common;

#[tokio::test]
async fn basic() {
    setup_sim(1);
}
