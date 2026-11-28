//! The screenshot cache (#114): a screen that hasn't changed isn't captured or encoded again.
//!
//! A framebuffer surface carries a seed (`IOSurfaceGetSeed`) that changes whenever its contents
//! change, so the last encoded image of each display is kept with the seed it was read at, and a
//! screenshot whose seed is the same answers with those bytes. A seed can move without a visible
//! change (iOS 26.5 ticks it 10 times a second on a still screen): then the contents are hashed
//! ([`hash_rows`]) and compared with the kept image's, so an unchanged picture is still not
//! encoded again. Either way a stale image is never returned: what isn't proven unchanged is
//! captured and encoded.

use std::sync::Arc;

use crate::frame::Frame;

/// What identifies a surface's contents without reading them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// The surface's ID (`IOSurfaceGetID`): another surface is another picture.
    pub surface: u32,
    /// Changes whenever the contents change (`IOSurfaceGetSeed`).
    pub seed: u32,
    pub width: u32,
    pub height: u32,
}

/// A display's surface, as the cache reads it.
pub trait Source {
    /// The surface's identity and seed now. Cheap: no pixels are read.
    fn stamp(&mut self) -> Result<Stamp, String>;
    /// The contents' [`hash_rows`] and the seed they were read at (one lock).
    fn hash(&mut self) -> Result<(u32, u64), String>;
    /// The contents as a frame, with the seed they were read at and their [`hash_rows`] (one lock).
    fn capture(&mut self) -> Result<(Frame, u32, u64), String>;
}

/// An encoded screenshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub bytes: Arc<Vec<u8>>,
    /// Answered from the cache: nothing was encoded.
    pub cached: bool,
}

/// What [`ShotCache::lookup`] found: the kept image, or a frame to encode and [`ShotCache::store`].
#[derive(Debug)]
pub enum Lookup<K> {
    Hit(Shot),
    Miss(Frame, Ticket<K>),
}

/// What a missed lookup read, to keep beside the bytes encoded from its frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket<K> {
    key: K,
    stamp: Stamp,
    turns: u64,
    hash: u64,
}

#[derive(Debug)]
struct Entry<K> {
    ticket: Ticket<K>,
    shot: Shot,
}

/// The last encoded image of each key (a display and a format, say), at most one per key.
#[derive(Debug)]
pub struct ShotCache<K> {
    entries: Vec<Entry<K>>,
}

impl<K> Default for ShotCache<K> {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl<K: PartialEq + Clone> ShotCache<K> {
    /// The image for `key` if the surface still shows what it was encoded from, else the frame
    /// to encode. `turns` counts the rotations and folds sent so far: a kept image from before
    /// one is dropped, as is one from another surface or of another size.
    pub fn lookup(&mut self, key: K, turns: u64, source: &mut impl Source) -> Result<Lookup<K>, String> {
        let stamp = source.stamp()?;
        if let Some(i) = self.entries.iter().position(|e| e.ticket.key == key) {
            let kept = &mut self.entries[i];
            let same = |a: &Stamp, b: &Stamp| (a.surface, a.width, a.height) == (b.surface, b.width, b.height);
            if !same(&kept.ticket.stamp, &stamp) || kept.ticket.turns != turns {
                self.entries.swap_remove(i);
            } else if kept.ticket.stamp.seed == stamp.seed {
                return Ok(Lookup::Hit(Shot { cached: true, ..kept.shot.clone() }));
            } else {
                // The seed moved: the picture may not have. The hash and its seed come from one
                // read, so a later change moves the seed past the one kept here.
                let (seed, hash) = source.hash()?;
                if hash == kept.ticket.hash {
                    kept.ticket.stamp.seed = seed;
                    return Ok(Lookup::Hit(Shot { cached: true, ..kept.shot.clone() }));
                }
            }
        }
        // The seed is the one the pixels were read at: never newer than they are.
        let (frame, seed, hash) = source.capture()?;
        Ok(Lookup::Miss(frame, Ticket { key, stamp: Stamp { seed, ..stamp }, turns, hash }))
    }

    /// Keeps `bytes`, encoded from a missed lookup's frame, for its key (replacing what it had).
    pub fn store(&mut self, ticket: Ticket<K>, width: u32, height: u32, bytes: Vec<u8>) -> Shot {
        let shot = Shot { width, height, bytes: Arc::new(bytes), cached: false };
        self.entries.retain(|e| e.ticket.key != ticket.key);
        self.entries.push(Entry { ticket, shot: shot.clone() });
        shot
    }

    /// [`ShotCache::lookup`], encoding a miss with `encode` and storing it.
    pub fn get(
        &mut self,
        key: K,
        turns: u64,
        source: &mut impl Source,
        encode: impl FnOnce(&Frame) -> Result<Vec<u8>, String>,
    ) -> Result<Shot, String> {
        match self.lookup(key, turns, source)? {
            Lookup::Hit(shot) => Ok(shot),
            Lookup::Miss(frame, ticket) => {
                let bytes = encode(&frame)?;
                Ok(self.store(ticket, frame.width, frame.height, bytes))
            }
        }
    }

    /// How many images are kept.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A 64-bit hash of `height` rows of `row` bytes each, `stride` bytes apart: the bytes past each
/// row's pixels don't count. `None` when `bytes` is too short. Four independent xxHash64-style
/// lanes: a 1206x2622 BGRA surface (12.6 MB) took 0.76 ms in memory on an Apple silicon host
/// (`hash_speed`; not yet measured on a live surface). It is not cryptographic: it tells a frame
/// from the one before it, of the same size.
pub fn hash_rows(bytes: &[u8], row: usize, height: usize, stride: usize) -> Option<u64> {
    const P1: u64 = 0x9e37_79b1_85eb_ca87;
    const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
    const P3: u64 = 0x1656_67b1_9e37_79f9;
    const P4: u64 = 0x85eb_ca77_c2b2_ae63;
    fn round(acc: u64, word: &[u8; 8]) -> u64 {
        acc.wrapping_add(u64::from_le_bytes(*word).wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
    }
    let fits = height == 0 || stride.checked_mul(height - 1).and_then(|n| n.checked_add(row)).is_some_and(|n| bytes.len() >= n);
    if stride < row || !fits {
        return None;
    }
    let mut lanes = [P1.wrapping_add(P2), P2, 0, P1.wrapping_neg()];
    for y in 0..height {
        let (blocks, rest) = bytes[y * stride..y * stride + row].as_chunks::<32>();
        for block in blocks {
            let words = block.as_chunks::<8>().0;
            for (lane, word) in lanes.iter_mut().zip(words) {
                *lane = round(*lane, word);
            }
        }
        let (words, tail) = rest.as_chunks::<8>();
        for (lane, word) in lanes.iter_mut().zip(words) {
            *lane = round(*lane, word);
        }
        if !tail.is_empty() {
            let mut word = [0u8; 8];
            word[..tail.len()].copy_from_slice(tail);
            lanes[3] = round(lanes[3], &word);
        }
    }
    let [a, b, c, d] = lanes;
    let mut h = a.rotate_left(1).wrapping_add(b.rotate_left(7)).wrapping_add(c.rotate_left(12)).wrapping_add(d.rotate_left(18));
    h = (h ^ (row as u64).wrapping_mul(P3)).wrapping_mul(P1) ^ (height as u64).wrapping_mul(P4);
    // xxHash64's avalanche.
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    Some(h ^ (h >> 32))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::frame::encode_png;
    use crate::reuse::{BootKey, Reuse};

    /// A display whose surface the test changes, counting what the cache reads.
    struct Fake {
        stamp: Stamp,
        /// The seed `stamp` reports moves without the picture changing (iOS 26.5).
        frame: Frame,
        captures: u32,
        hashes: u32,
    }

    impl Fake {
        fn new() -> Self {
            Self { stamp: Stamp { surface: 7, seed: 1, width: 2, height: 1 }, frame: frame(0), captures: 0, hashes: 0 }
        }

        /// The device renders a new picture.
        fn draw(&mut self, shade: u8) {
            self.frame = frame(shade);
            self.stamp.seed += 1;
        }
    }

    fn frame(shade: u8) -> Frame {
        Frame { width: 2, height: 1, rgba: vec![shade, 2, 3, 255, 4, shade, 6, 255] }
    }

    fn hash_of(f: &Frame) -> u64 {
        hash_rows(&f.rgba, f.width as usize * 4, f.height as usize, f.width as usize * 4).expect("fits")
    }

    impl Source for Fake {
        fn stamp(&mut self) -> Result<Stamp, String> {
            Ok(self.stamp)
        }
        fn hash(&mut self) -> Result<(u32, u64), String> {
            self.hashes += 1;
            Ok((self.stamp.seed, hash_of(&self.frame)))
        }
        fn capture(&mut self) -> Result<(Frame, u32, u64), String> {
            self.captures += 1;
            Ok((self.frame.clone(), self.stamp.seed, hash_of(&self.frame)))
        }
    }

    /// An encoder that counts its calls and encodes for real.
    fn counting(n: &Cell<u32>) -> impl FnOnce(&Frame) -> Result<Vec<u8>, String> + '_ {
        move |f| {
            n.set(n.get() + 1);
            encode_png(f)
        }
    }

    const MAIN: (usize, &str) = (0, "png");

    #[test]
    fn an_unchanged_seed_answers_from_the_cache_without_encoding() {
        let (mut cache, mut d, encodes) = (ShotCache::default(), Fake::new(), Cell::new(0));
        let first = cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("first");
        let second = cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("second");
        assert_eq!(encodes.get(), 1, "the second screenshot of an unchanged screen isn't encoded");
        assert_eq!(d.captures, 1, "nor captured");
        assert!(!first.cached && second.cached);
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn a_new_seed_with_a_new_picture_encodes_again() {
        let (mut cache, mut d, encodes) = (ShotCache::default(), Fake::new(), Cell::new(0));
        let before = cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("before");
        assert!(cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("kept").cached);
        d.draw(9);
        let after = cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("after");
        assert!(!after.cached);
        assert_eq!(encodes.get(), 2);
        assert_ne!(before.bytes, after.bytes);
        assert_eq!(*after.bytes, encode_png(&frame(9)).expect("png"), "the new picture, not the kept one");
    }

    #[test]
    fn a_seed_that_moves_on_a_still_screen_is_hashed_not_encoded() {
        // iOS 26.5 ticks the seed 10 times a second whether or not anything changed.
        let (mut cache, mut d, encodes) = (ShotCache::default(), Fake::new(), Cell::new(0));
        cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("first");
        d.stamp.seed += 1;
        let again = cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("again");
        assert_eq!((encodes.get(), d.captures, d.hashes), (1, 1, 1), "hashed once, not captured or encoded");
        assert!(again.cached);
        // The seed it was hashed at is kept: the next call with it reads nothing.
        cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("third");
        assert_eq!((encodes.get(), d.captures, d.hashes), (1, 1, 1));
    }

    #[test]
    fn a_rotation_drops_the_kept_image() {
        let (mut cache, mut d, encodes) = (ShotCache::default(), Fake::new(), Cell::new(0));
        cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("before");
        assert!(cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("kept").cached);
        // Rotating turns the picture, not the surface: same ID, size and (here) seed.
        let turned = cache.get(MAIN, 1, &mut d, counting(&encodes)).expect("after a rotation");
        assert_eq!(encodes.get(), 2);
        assert!(!turned.cached);
        assert_eq!(cache.len(), 1, "replaced, not added");
    }

    #[test]
    fn another_surface_or_size_drops_the_kept_image() {
        let (mut cache, mut d, encodes) = (ShotCache::default(), Fake::new(), Cell::new(0));
        assert!(!cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("first").cached);
        assert!(cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("kept").cached);
        d.stamp.surface = 8;
        assert!(!cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("new surface").cached);
        d.stamp.width = 4;
        assert!(!cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("new size").cached);
        assert_eq!(encodes.get(), 3, "no hash can stand in for another surface: each is encoded");
        assert_eq!(d.hashes, 0);
    }

    #[test]
    fn a_reboot_starts_with_an_empty_cache() {
        // The iOS worker keeps the cache with the displays it reuses for one boot.
        let mut kept: Reuse<BootKey, Cell<ShotCache<(usize, &str)>>> = Reuse::default();
        let (mut d, encodes) = (Fake::new(), Cell::new(0));
        let shoot = |kept: &mut Reuse<_, Cell<ShotCache<_>>>, key: Option<BootKey>, d: &mut Fake| {
            kept.with(
                key,
                || Ok::<_, String>(Cell::default()),
                |c| {
                    let mut cache = c.take();
                    let shot = cache.get(MAIN, 0, d, counting(&encodes));
                    c.set(cache);
                    shot
                },
            )
        };
        let boot = |n| BootKey::of(true, Some(n), None);
        assert!(!shoot(&mut kept, boot(1), &mut d).expect("first").cached);
        assert!(shoot(&mut kept, boot(1), &mut d).expect("same boot").cached);
        assert!(!shoot(&mut kept, boot(2), &mut d).expect("rebooted").cached, "a new boot encodes again");
        assert_eq!(encodes.get(), 2);
    }

    #[test]
    fn one_image_is_kept_per_display_and_format() {
        let (mut cache, mut d, encodes) = (ShotCache::default(), Fake::new(), Cell::new(0));
        for shade in 0..5 {
            d.draw(shade);
            cache.get(MAIN, 0, &mut d, counting(&encodes)).expect("main");
            cache.get((1, "png"), 0, &mut d, counting(&encodes)).expect("inner");
        }
        assert_eq!(cache.len(), 2);
        assert!(cache.get((1, "png"), 0, &mut d, counting(&encodes)).expect("inner").cached, "keys don't evict each other");
    }

    #[test]
    fn a_screenshot_is_the_same_png_bytes_cached_or_not() {
        let (mut cache, mut d) = (ShotCache::default(), Fake::new());
        let direct = encode_png(&frame(0)).expect("png");
        let first = cache.get(MAIN, 0, &mut d, encode_png).expect("first");
        let second = cache.get(MAIN, 0, &mut d, encode_png).expect("second");
        assert_eq!((*first.bytes).clone(), direct);
        assert_eq!((*second.bytes).clone(), direct);
        assert_eq!((first.width, first.height), (2, 1));
    }

    #[test]
    fn a_failed_encode_keeps_nothing() {
        let (mut cache, mut d) = (ShotCache::default(), Fake::new());
        assert!(cache.get(MAIN, 0, &mut d, |_| Err("no".into())).is_err());
        assert!(cache.is_empty());
    }

    #[test]
    fn the_row_hash_reads_only_each_rows_pixels() {
        // Two rows of 4 bytes, 6 bytes apart: the padding doesn't count, the pixels do.
        let a = [1, 2, 3, 4, 0xaa, 0xaa, 5, 6, 7, 8];
        let b = [1, 2, 3, 4, 0xbb, 0xbb, 5, 6, 7, 8];
        let c = [1, 2, 3, 4, 0xaa, 0xaa, 5, 6, 7, 9];
        assert_eq!(hash_rows(&a, 4, 2, 6), hash_rows(&b, 4, 2, 6));
        assert_ne!(hash_rows(&a, 4, 2, 6), hash_rows(&c, 4, 2, 6));
        assert_eq!(hash_rows(&a, 4, 2, 7), None, "too short");
        // Every byte of a long row counts, in blocks, words and the tail alike.
        let base: Vec<u8> = (0..77u8).collect();
        let h = hash_rows(&base, 77, 1, 77);
        for i in 0..base.len() {
            let mut changed = base.clone();
            changed[i] ^= 1;
            assert_ne!(hash_rows(&changed, 77, 1, 77), h, "byte {i}");
        }
        assert_ne!(hash_rows(&base, 76, 1, 77), h, "the row length counts");
    }

    /// What a hash of an iPhone 17 Pro's surface costs (1206x2622 BGRA, 4864 bytes a row): the price
    /// of a moved seed on a still screen. `cargo test --release -p simee-core --lib hash_speed --
    /// --ignored --nocapture`.
    #[test]
    #[ignore = "a timing, not a check"]
    fn hash_speed() {
        let (w, h, stride) = (1206usize, 2622usize, 4864usize);
        let bytes: Vec<u8> = (0..stride * h).map(|i| (i * 31 % 251) as u8).collect();
        let best = (0..20)
            .map(|_| {
                let t = std::time::Instant::now();
                std::hint::black_box(hash_rows(std::hint::black_box(&bytes), w * 4, h, stride));
                t.elapsed()
            })
            .min()
            .expect("runs");
        eprintln!("hash_rows of a 1206x2622 BGRA surface, best of 20: {best:?}");
    }
}
