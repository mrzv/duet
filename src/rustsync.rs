// [package]
// name = "rustsync"
// version = "0.2.3"
// description = "A pure Rust implementation of rsync"
// license = "Apache-2.0/MIT"
// authors = ["pe@pijul.org <pe@pijul.org>"]
// include = [ "Cargo.toml", "src/lib.rs" ]
// documentation = "https://docs.rs/rustsync"
// repository = "https://nest.pijul.com/pmeunier/rustsync"

//! An implementation of an rsync-like protocol (not compatible with
//! rsync), in pure Rust.
//!
//! ```
//! extern crate rand;
//! extern crate rustsync;
//! use rustsync::*;
//! use rand::Rng;
//! fn main() {
//!   // Create 4 different random strings first.
//!   let chunk_size = 1000;
//!   let a = rand::thread_rng()
//!           .gen_ascii_chars()
//!           .take(chunk_size)
//!           .collect::<String>();
//!   let b = rand::thread_rng()
//!           .gen_ascii_chars()
//!           .take(50)
//!           .collect::<String>();
//!   let b_ = rand::thread_rng()
//!           .gen_ascii_chars()
//!           .take(100)
//!           .collect::<String>();
//!   let c = rand::thread_rng()
//!           .gen_ascii_chars()
//!           .take(chunk_size)
//!           .collect::<String>();
//!
//!   // Now concatenate them in two different ways.
//!
//!   let mut source = a.clone() + &b + &c;
//!   let mut modified = a + &b_ + &c;
//!
//!   // Suppose we want to download `modified`, and we already have
//!   // `source`, which only differs by a few characters in the
//!   // middle.
//!
//!   // We first have to choose a block size, which will be recorded
//!   // in the signature below. Blocks should normally be much bigger
//!   // than this in order to be efficient on large files.
//!
//!   let block = [0; 32];
//!
//!   // We then create a signature of `source`, to be uploaded to the
//!   // remote machine. Signatures are typically much smaller than
//!   // files, with just a few bytes per block.
//!
//!   let source_sig = signature(source.as_bytes(), block).unwrap();
//!
//!   // Then, we let the server compare our signature with their
//!   // version.
//!
//!   let comp = compare(&source_sig, modified.as_bytes(), block).unwrap();
//!
//!   // We finally download the result of that comparison, and
//!   // restore their file from that.
//!
//!   let mut restored = Vec::new();
//!   restore_seek(&mut restored, std::io::Cursor::new(source.as_bytes()), vec![0; 1000], &comp).unwrap();
//!   assert_eq!(&restored[..], modified.as_bytes())
//! }
//! ```

extern crate adler32;
extern crate blake2_rfc;
extern crate futures;
#[cfg(test)]
extern crate rand;
extern crate serde;
//#[macro_use]
//extern crate serde_derive;

use std::collections::HashMap;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};

const BLAKE2_SIZE: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Blake2b([u8; BLAKE2_SIZE]);

impl std::borrow::Borrow<[u8]> for Blake2b {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
/// The "signature" of the file, which is essentially a
/// content-indexed description of the blocks in the file.
pub struct Signature {
    pub window: usize,
    chunks: HashMap<u32, HashMap<Blake2b, usize>>,
}

/// Create the "signature" of a file, essentially a content-indexed
/// map of blocks. The first step of the protocol is to run this
/// function on the "source" (the remote file when downloading, the
/// local file while uploading).
pub fn signature<R: Read, B: AsRef<[u8]> + AsMut<[u8]>>(
    mut r: R,
    mut block: B,
) -> Result<Signature, std::io::Error> {
    let mut chunks = HashMap::new();

    let mut i = 0;
    let block = block.as_mut();
    let mut eof = false;
    while !eof {
        let mut j = 0;
        while j < block.len() {
            let r = r.read(&mut block[j..])?;
            if r == 0 {
                eof = true;
                break;
            }
            j += r
        }
        let block = &block[..j];
        let hash = adler32::RollingAdler32::from_buffer(block);
        let mut blake2 = [0; BLAKE2_SIZE];
        blake2.clone_from_slice(blake2_rfc::blake2b::blake2b(BLAKE2_SIZE, &[], &block).as_bytes());
        //println!("{:?} {:?}", block, blake2);
        chunks
            .entry(hash.hash())
            .or_insert(HashMap::new())
            .insert(Blake2b(blake2), i);

        i += block.len()
    }

    Ok(Signature {
        window: block.len(),
        chunks,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Block {
    FromSource(u64),
    Literal(Vec<u8>),
}

struct State {
    block_oldest: usize,
    block_len: usize,
    pending: Vec<u8>,
}

impl State {
    fn new() -> Self {
        State {
            block_oldest: 0,
            block_len: 1,
            pending: Vec::new(),
        }
    }
}

#[derive(Default, Debug, Serialize, Deserialize, PartialEq)]
/// The result of comparing two files
pub struct Delta {
    /// Description of the new file in terms of blocks.
    pub blocks: Vec<Block>,
    /// Size of the window.
    pub window: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeltaOp {
    FromSource(u64),
    Literal(Vec<u8>),
}

/// Compare a signature with an existing file. This is the second step
/// of the protocol, `r` is the local file when downloading, and the
/// remote file when uploading.
///
/// `block` must be a buffer the same size as `sig.window`.
pub fn compare<R: Read, B: AsRef<[u8]> + AsMut<[u8]>>(
    sig: &Signature,
    mut r: R,
    mut block: B,
) -> Result<Delta, std::io::Error> {
    let mut blocks = Vec::new();
    compare_stream(sig, &mut r, &mut block, usize::MAX, |op| {
        match op {
            DeltaOp::FromSource(offset) => blocks.push(Block::FromSource(offset)),
            DeltaOp::Literal(bytes) => blocks.push(Block::Literal(bytes)),
        }
        Ok(())
    })?;

    Ok(Delta {
        blocks,
        window: sig.window,
    })
}

pub fn compare_stream<R, B, F>(
    sig: &Signature,
    mut r: R,
    mut block: B,
    max_literal_bytes: usize,
    mut emit: F,
) -> Result<(), std::io::Error>
where
    R: Read,
    B: AsRef<[u8]> + AsMut<[u8]>,
    F: FnMut(DeltaOp) -> Result<(), std::io::Error>,
{
    let mut st = State::new();
    let block = block.as_mut();
    if sig.window == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "signature window must be non-zero",
        ));
    }
    if block.len() != sig.window {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "signature window {} does not match buffer length {}",
                sig.window,
                block.len()
            ),
        ));
    }
    let max_literal_bytes = max_literal_bytes.max(1);

    while st.block_len > 0 {
        let mut hash = {
            let mut j = 0;
            let block = {
                while j < sig.window {
                    let r = r.read(&mut block[j..sig.window])?;
                    if r == 0 {
                        break;
                    }
                    j += r
                }
                st.block_oldest = 0;
                st.block_len = j;
                &block[..j]
            };
            adler32::RollingAdler32::from_buffer(block)
        };

        // Starting from the current block (with hash `hash`), find
        // the next block with a hash that appears in the signature.
        loop {
            if let Some(index) = matching_index(&st, sig, &block, &hash) {
                if !st.pending.is_empty() {
                    emit(DeltaOp::Literal(std::mem::replace(
                        &mut st.pending,
                        Vec::new(),
                    )))?;
                }
                emit(DeltaOp::FromSource(index as u64))?;
                break;
            }
            // The blocks are not equal. Move the hash by one byte
            // until finding an equal block.
            let oldest = block[st.block_oldest];
            hash.remove(st.block_len, oldest);
            let r = r.read(&mut block[st.block_oldest..st.block_oldest + 1])?;
            if r > 0 {
                // If there are still bytes to read, update the hash.
                hash.update(block[st.block_oldest]);
            } else if st.block_len > 0 {
                // Else, just shrink the window, so that the current
                // block's blake2 hash can be compared with the
                // signature.
                st.block_len -= 1;
            } else {
                // We're done reading the file.
                break;
            }
            st.pending.push(oldest);
            if st.pending.len() >= max_literal_bytes {
                emit(DeltaOp::Literal(std::mem::replace(
                    &mut st.pending,
                    Vec::new(),
                )))?;
            }
            st.block_oldest = (st.block_oldest + 1) % sig.window;
        }
        if !st.pending.is_empty() {
            // We've reached the end of the file, and have never found
            // a matching block again.
            emit(DeltaOp::Literal(std::mem::replace(
                &mut st.pending,
                Vec::new(),
            )))?;
        }
    }

    Ok(())
}

fn matching_index(
    st: &State,
    sig: &Signature,
    block: &[u8],
    hash: &adler32::RollingAdler32,
) -> Option<usize> {
    if let Some(h) = sig.chunks.get(&hash.hash()) {
        let blake2 = {
            let mut b = blake2_rfc::blake2b::Blake2b::new(BLAKE2_SIZE);
            if st.block_oldest + st.block_len > sig.window {
                b.update(&block[st.block_oldest..]);
                b.update(&block[..(st.block_oldest + st.block_len) % sig.window]);
            } else {
                b.update(&block[st.block_oldest..st.block_oldest + st.block_len])
            }
            b.finalize()
        };

        if let Some(&index) = h.get(blake2.as_bytes()) {
            return Some(index);
        }
    }
    None
}

/// Restore a file, using a "delta" (resulting from
/// [`compare`](fn.compare.html))
#[allow(dead_code)]
pub fn restore<W: Write>(mut w: W, s: &[u8], delta: &Delta) -> Result<(), std::io::Error> {
    for d in delta.blocks.iter() {
        match *d {
            Block::FromSource(i) => {
                let i = i as usize;
                if i + delta.window <= s.len() {
                    w.write(&s[i..i + delta.window])?
                } else {
                    w.write(&s[i..])?
                }
            }
            Block::Literal(ref l) => w.write(l)?,
        };
    }
    Ok(())
}

/// Same as [`restore`](fn.restore.html), except that this function
/// uses a seekable, readable stream instead of the entire file in a
/// slice.
///
/// `buf` must be a buffer the same size as `sig.window`.
pub fn restore_seek<W: Write, R: Read + Seek, B: AsRef<[u8]> + AsMut<[u8]>>(
    mut w: W,
    mut s: R,
    mut buf: B,
    delta: &Delta,
) -> Result<(), std::io::Error> {
    let buf = buf.as_mut();
    if delta.window == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "delta window must be non-zero",
        ));
    }
    if buf.len() < delta.window {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "delta window {} exceeds buffer length {}",
                delta.window,
                buf.len()
            ),
        ));
    }

    for d in delta.blocks.iter() {
        match *d {
            Block::FromSource(i) => {
                s.seek(SeekFrom::Start(i as u64))?;
                // fill the buffer from r.
                let mut n = 0;
                loop {
                    let r = s.read(&mut buf[n..delta.window])?;
                    if r == 0 {
                        break;
                    }
                    n += r;
                    if n == delta.window {
                        break;
                    }
                }
                // write the buffer to w.
                w.write_all(&buf[..n])?;
            }
            Block::Literal(ref l) => {
                w.write_all(l)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::distributions::{Alphanumeric, DistString};
    use std::cmp;
    const WINDOW: usize = 32;

    struct ShortReader<R> {
        inner: R,
        max_read: usize,
    }

    impl<R: Read> Read for ShortReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
            let len = cmp::min(buf.len(), self.max_read);
            self.inner.read(&mut buf[..len])
        }
    }

    #[test]
    fn basic() {
        for index in 0..10 {
            let source = Alphanumeric.sample_string(&mut rand::thread_rng(), WINDOW * 10 + 8);
            let mut modified = source.clone();
            let index = WINDOW * index + 3;
            unsafe {
                modified.as_bytes_mut()[index] =
                    ((source.as_bytes()[index] as usize + 1) & 255) as u8
            }
            let block = [0; WINDOW];
            let source_sig = signature(source.as_bytes(), block).unwrap();
            let comp = compare(&source_sig, modified.as_bytes(), block).unwrap();

            let mut restored = Vec::new();
            let source = std::io::Cursor::new(source.as_bytes());
            restore_seek(&mut restored, source, [0; WINDOW], &comp).unwrap();
            if &restored[..] != modified.as_bytes() {
                for i in 0..10 {
                    let a = &restored[i * WINDOW..(i + 1) * WINDOW];
                    let b = &modified.as_bytes()[i * WINDOW..(i + 1) * WINDOW];
                    println!("{:?}\n{:?}\n", a, b);
                    if a != b {
                        println!(">>>>>>>>");
                    }
                }
                panic!("different");
            }
        }
    }

    #[test]
    fn compare_stream_handles_short_reads() {
        let source = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let modified = b"abcdefghijklmnopqrstuvwxyz9876543210";
        let sig = signature(&source[..], [0; 8]).unwrap();
        let reader = ShortReader {
            inner: &modified[..],
            max_read: 3,
        };

        let delta = compare(&sig, reader, [0; 8]).unwrap();
        let mut restored = Vec::new();
        restore_seek(&mut restored, std::io::Cursor::new(source), [0; 8], &delta).unwrap();

        assert_eq!(&restored, modified);
    }

    #[test]
    fn compare_stream_rejects_invalid_buffers() {
        let sig = Signature {
            window: 8,
            chunks: HashMap::new(),
        };
        let err = compare_stream(&sig, &b"data"[..], [0; 4], usize::MAX, |_| Ok(())).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);

        let sig = Signature {
            window: 0,
            chunks: HashMap::new(),
        };
        let err = compare_stream(&sig, &b"data"[..], [], usize::MAX, |_| Ok(())).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }

    #[test]
    fn restore_seek_rejects_invalid_buffers() {
        let delta = Delta {
            blocks: Vec::new(),
            window: 8,
        };
        let err =
            restore_seek(Vec::new(), std::io::Cursor::new(Vec::new()), [0; 4], &delta).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);

        let delta = Delta {
            blocks: Vec::new(),
            window: 0,
        };
        let err =
            restore_seek(Vec::new(), std::io::Cursor::new(Vec::new()), [], &delta).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
    }
}
