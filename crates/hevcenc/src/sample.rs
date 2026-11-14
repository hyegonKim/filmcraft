//! The sample type of the encoder's pictures: `u16` for every bit depth.

/// What intra prediction needs of a sample type.
pub trait Sample: Copy {
    fn from_u16(v: u16) -> Self;
    fn from_i32(v: i32) -> Self;
}

impl Sample for u16 {
    #[inline]
    fn from_u16(v: u16) -> Self {
        v
    }
    #[inline]
    fn from_i32(v: i32) -> Self {
        v.clamp(0, i32::from(u16::MAX)) as u16
    }
}
