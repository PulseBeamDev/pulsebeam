mod common;

use test_strategy::{Arbitrary, proptest};

#[derive(Arbitrary, Debug)]
struct TestInputStruct {
    #[strategy(1..10u32)]
    a: u32,

    #[strategy(0..#a)]
    b: u32,
}

#[proptest]
fn my_test(input: TestInputStruct) {
    assert_eq!(common::add(input.a, input.b), input.a + input.b);
}
