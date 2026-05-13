#[macro_export]
macro_rules! cfg_loom {
    ($($item:item)*) => {
        $( #[cfg(any(test, feature = "loom"))] $item )*
    }
}

#[macro_export]
macro_rules! cfg_not_loom {
    ($($item:item)*) => {
        $( #[cfg(not(feature = "loom"))] $item )*
    }
}

#[macro_export]
macro_rules! cfg_sim {
    ($($item:item)*) => {
        $( #[cfg(feature = "sim")] $item )*
    }
}

#[macro_export]
macro_rules! cfg_not_sim {
    ($($item:item)*) => {
        $( #[cfg(not(feature = "sim"))] $item )*
    }
}
