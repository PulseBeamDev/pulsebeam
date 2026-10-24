//! An unknown payload which cannot safely be disposed by retrying its Drop.
struct Field;
impl Drop for Field {
    fn drop(&mut self) {
        panic!("payload field cleanup panic");
    }
}
struct Payload(Field);
impl Drop for Payload {
    fn drop(&mut self) {
        panic!("payload destructor panic");
    }
}
pub fn raise() -> ! {
    std::panic::panic_any(Payload(Field));
}
