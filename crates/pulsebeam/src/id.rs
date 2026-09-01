use std::borrow::Borrow;
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct ShardId(usize);

impl ShardId {
    pub const fn new(index: usize) -> Self {
        Self(index)
    }

    pub const fn index(self) -> usize {
        self.0
    }
}

impl From<usize> for ShardId {
    fn from(index: usize) -> Self {
        Self::new(index)
    }
}

impl From<ShardId> for usize {
    fn from(id: ShardId) -> Self {
        id.index()
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Borrow<usize> for ShardId {
    fn borrow(&self) -> &usize {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct AudioSelectorSlotId(usize);

impl AudioSelectorSlotId {
    pub const fn new(index: usize) -> Self {
        Self(index)
    }

    pub const fn index(self) -> usize {
        self.0
    }
}

impl From<usize> for AudioSelectorSlotId {
    fn from(index: usize) -> Self {
        Self::new(index)
    }
}

impl From<AudioSelectorSlotId> for usize {
    fn from(id: AudioSelectorSlotId) -> Self {
        id.index()
    }
}

impl fmt::Display for AudioSelectorSlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Borrow<usize> for AudioSelectorSlotId {
    fn borrow(&self) -> &usize {
        &self.0
    }
}
