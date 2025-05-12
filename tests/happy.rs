use common::{new_rt, setup_sim};
mod common;

#[test]
fn basic() {
    let rt = new_rt(1);
    rt.block_on(async move {
        setup_sim(1).await;
    })
}
