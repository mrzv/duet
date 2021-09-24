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
use std::io::{Read, Seek, SeekFrom, Write};

const BLAKE2_SIZE: usize = 32;

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Blake2b([u8; BLAKE2_SIZE]);

impl std::borrow::Borrow<[u8]> for Blake2b {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
/// The "signature" of the file, which is essentially a
/// content-indexed description of the blocks in the file.
pub struct Signature {
    pub window: usize,
    chunks: HashMap<u32, HashMap<Blake2b, usize>>
}

/// Create the "signature" of a file, essentially a content-indexed
/// map of blocks. The first step of the protocol is to run this
/// function on the "source" (the remote file when downloading, the
/// local file while uploading).
pub fn signature<R: Read, B: AsRef<[u8]>+AsMut<[u8]>>(mut r: R, mut block: B) -> Result<Signature, std::io::Error> {
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
        chunks
    })
}


#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum Block {
    FromSource(u64),
    Literal(Vec<u8>),
}

struct State {
    result: Vec<Block>,
    block_oldest: usize,
    block_len: usize,
    pending: Vec<u8>,
}

impl State {
    fn new() -> Self {
        State {
            result: Vec::new(),
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

/// Compare a signature with an existing file. This is the second step
/// of the protocol, `r` is the local file when downloading, and the
/// remote file when uploading.
///
/// `block` must be a buffer the same size as `sig.window`.
pub fn compare<R: Read, B:AsRef<[u8]>+AsMut<[u8]>>(sig: &Signature, mut r: R, mut block: B) -> Result<Delta, std::io::Error> {
    let mut st = State::new();
    let block = block.as_mut();
    assert_eq!(block.len(), sig.window);
    while st.block_len > 0 {
        let mut hash = {
            let mut j = 0;
            let block = {
                while j < sig.window {
                    let r = r.read(&mut block[..])?;
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
            if matches(&mut st, sig, &block, &hash) {
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
            st.block_oldest = (st.block_oldest + 1) % sig.window;
        }
        if !st.pending.is_empty() {
            // We've reached the end of the file, and have never found
            // a matching block again.
            st.result.push(Block::Literal(std::mem::replace(
                &mut st.pending,
                Vec::new(),
            )))
        }
    }
    Ok(Delta {
        blocks: st.result,
        window: sig.window,
    })
}

fn matches(st: &mut State, sig: &Signature, block: &[u8], hash: &adler32::RollingAdler32) -> bool {
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
            // Matching hash found! If we have non-matching
            // material before the match, add it.
            if !st.pending.is_empty() {
                st.result.push(Block::Literal(std::mem::replace(
                    &mut st.pending,
                    Vec::new(),
                )));
            }
            st.result.push(Block::FromSource(index as u64));
            return true;
        }
    }
    false
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
pub fn restore_seek<W: Write, R: Read + Seek, B: AsRef<[u8]>+AsMut<[u8]>>(
    mut w: W,
    mut s: R,
    mut buf: B,
    delta: &Delta,
) -> Result<(), std::io::Error> {
    let buf = buf.as_mut();

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
                    n += r
                }
                // write the buffer to w.
                let mut m = 0;
                while m < n {
                    m += w.write(&buf[m..n])?;
                }
            }
            Block::Literal(ref l) => {
                w.write(l)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use rand;
    use super::*;
    use rand::Rng;
    const WINDOW: usize = 32;
    #[test]
    fn basic() {
        for index in 0..10 {
            let source = rand::thread_rng()
                .gen_ascii_chars()
                .take(WINDOW * 10 + 8)
                .collect::<String>();
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
}
