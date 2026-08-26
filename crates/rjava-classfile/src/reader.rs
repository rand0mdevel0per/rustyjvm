//! A minimal big-endian cursor over class-file bytes. Every read is bounds-checked and returns a
//! [`ClassFileError::Eof`] rather than panicking, so adversarial/truncated input is handled
//! safely (RJVM-SPEC-001 §4.3, §23.3).

use crate::error::ClassFileError;

/// Big-endian byte reader over a class-file slice.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// Read one `u1`.
    pub fn u1(&mut self) -> Result<u8, ClassFileError> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or(ClassFileError::Eof(self.pos))?;
        self.pos += 1;
        Ok(b)
    }

    /// Read a big-endian `u2`.
    pub fn u2(&mut self) -> Result<u16, ClassFileError> {
        let hi = self.u1()? as u16;
        let lo = self.u1()? as u16;
        Ok((hi << 8) | lo)
    }

    /// Read a big-endian `u4`.
    pub fn u4(&mut self) -> Result<u32, ClassFileError> {
        let hi = self.u2()? as u32;
        let lo = self.u2()? as u32;
        Ok((hi << 16) | lo)
    }

    /// Read a big-endian `u8`.
    pub fn u8(&mut self) -> Result<u64, ClassFileError> {
        let hi = self.u4()? as u64;
        let lo = self.u4()? as u64;
        Ok((hi << 32) | lo)
    }

    /// Borrow the next `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], ClassFileError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(ClassFileError::Eof(self.pos))?;
        let s = self
            .data
            .get(self.pos..end)
            .ok_or(ClassFileError::Eof(self.pos))?;
        self.pos = end;
        Ok(s)
    }
}
