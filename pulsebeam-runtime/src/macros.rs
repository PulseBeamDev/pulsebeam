#[macro_export]
macro_rules! cfg_loom {
    ($($item:item)*) => {
        $( #[cfg(feature = "loom")] $item )*
    }
}

#[macro_export]
macro_rules! cfg_not_loom {
    ($($item:item)*) => {
        $( #[cfg(not(feature = "loom"))] $item )*
    }
}
