//! Reading a whole `.zst` stream, which may hold several frames.

use std::io::{self, BufRead, Read};

use ruzstd::decoding::{FrameDecoder, StreamingDecoder};

/// Decompresses every zstd frame in `source`, back to back. `zstd` writes
/// one frame per stream, but concatenated `.zst` files are valid too, and
/// ruzstd's [`StreamingDecoder`] stops after one frame.
pub(crate) struct Frames<R: BufRead> {
    state: State<R>,
}

enum State<R: BufRead> {
    /// Inside a frame.
    Frame(Box<StreamingDecoder<R, FrameDecoder>>),
    /// At a frame boundary (or the start).
    Between(R),
    /// After an error took the source.
    Failed,
}

impl<R: BufRead> Frames<R> {
    pub(crate) fn new(source: R) -> Self {
        Self {
            state: State::Between(source),
        }
    }
}

impl<R: BufRead> Read for Frames<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            match std::mem::replace(&mut self.state, State::Failed) {
                State::Frame(mut frame) => {
                    let n = frame.read(buf)?;
                    if n > 0 {
                        self.state = State::Frame(frame);
                        return Ok(n);
                    }
                    self.state = State::Between(frame.into_inner());
                }
                State::Between(mut source) => {
                    if source.fill_buf()?.is_empty() {
                        self.state = State::Between(source);
                        return Ok(0);
                    }
                    let frame = StreamingDecoder::new(source).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {e}"))
                    })?;
                    self.state = State::Frame(Box::new(frame));
                }
                State::Failed => {
                    return Err(io::Error::other("zstd stream failed earlier"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ruzstd::encoding::{CompressionLevel, compress_to_vec};

    use super::*;

    fn decompress(bytes: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        Frames::new(bytes).read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn reads_concatenated_frames() {
        let mut stream = compress_to_vec(&b"hello "[..], CompressionLevel::Fastest);
        stream.extend(compress_to_vec(&b"world"[..], CompressionLevel::Fastest));
        assert_eq!(decompress(&stream).expect("decodes"), b"hello world");
        assert_eq!(decompress(b"").expect("empty"), b"");
    }

    #[test]
    fn rejects_non_zstd() {
        let err = decompress(b"not zstd at all").expect_err("bad magic");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
