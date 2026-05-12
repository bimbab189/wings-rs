use std::io::Read;

pub struct FixedReader<R: Read> {
    inner: R,
    size: usize,
    bytes_read: usize,
}

impl<R: Read> FixedReader<R> {
    pub fn new_with_fixed_bytes(inner: R, size: usize) -> Self {
        Self {
            inner,
            size,
            bytes_read: 0,
        }
    }
}

impl<R: Read> Read for FixedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.bytes_read >= self.size {
            return Ok(0);
        }

        let remaining = self.size - self.bytes_read;
        let to_read = std::cmp::min(buf.len(), remaining);

        let n = self.inner.read(&mut buf[..to_read])?;

        if crate::unlikely(n == 0) {
            buf[..to_read].fill(0);

            self.bytes_read += to_read;
            return Ok(to_read);
        }

        self.bytes_read += n;
        Ok(n)
    }
}
