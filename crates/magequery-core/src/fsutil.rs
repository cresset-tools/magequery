//! Directory enumeration, with a test-only order shuffle.
//!
//! Nothing we compute may depend on the order a filesystem hands back
//! directory entries: APFS, ext4 and overlayfs all differ, so an order
//! dependence makes the output machine-specific while looking perfectly
//! stable on the machine you develop on. Two compiles of the same tree on
//! one laptop can never catch it.
//!
//! Sorting every read would hide such a dependence rather than disprove it
//! (and would impose an order Magento itself does not always use). So the
//! order is left alone, and instead it can be RANDOMIZED on demand: set
//! `MQ_SHUFFLE_READDIR=<seed>` (or call [`set_shuffle_seed`]) and every
//! enumeration below comes back permuted. A compile whose output is
//! byte-identical across several seeds does not depend on the order — that
//! is the assertion `readdir_order.rs` makes.
//!
//! The permutation is a pure function of (seed, directory path), so a run is
//! reproducible from its seed alone.

use std::fs::DirEntry;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// `UNSET` = not yet read from the environment; 0 = shuffling off.
const UNSET: u64 = u64::MAX;
static SEED: AtomicU64 = AtomicU64::new(UNSET);

/// Set the shuffle seed directly (0 = off), for tests that want several
/// orders inside one process. Overrides `MQ_SHUFFLE_READDIR`.
pub fn set_shuffle_seed(seed: u64) {
    SEED.store(seed, Ordering::Relaxed);
}

fn seed() -> u64 {
    let s = SEED.load(Ordering::Relaxed);
    if s != UNSET {
        return s;
    }
    let from_env = std::env::var("MQ_SHUFFLE_READDIR")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    SEED.store(from_env, Ordering::Relaxed);
    from_env
}

/// Read `dir`'s entries, dropping the ones that error (the `.flatten()` every
/// call site did for itself). In filesystem order unless a shuffle seed is
/// set. Collecting is deliberate: it is what makes the order interceptable,
/// and these directories are small.
pub fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<DirEntry>> {
    let mut entries: Vec<DirEntry> = std::fs::read_dir(dir)?.flatten().collect();
    let seed = seed();
    if seed != 0 {
        shuffle(&mut entries, seed, dir);
    }
    Ok(entries)
}

/// Fisher-Yates over SplitMix64, seeded by (seed, path) so one directory's
/// permutation does not depend on how many were read before it.
fn shuffle(entries: &mut [DirEntry], seed: u64, dir: &Path) {
    let mut state = seed ^ fnv1a(dir.to_string_lossy().as_bytes());
    for i in (1..entries.len()).rev() {
        let j = (next(&mut state) % (i as u64 + 1)) as usize;
        entries.swap(i, j);
    }
}

fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xCBF29CE484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_permutes_and_zero_does_not() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..12 {
            std::fs::write(dir.path().join(format!("f{n}")), b"x").unwrap();
        }
        let names = |seed: u64| {
            set_shuffle_seed(seed);
            read_dir_entries(dir.path())
                .unwrap()
                .iter()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        let plain = names(0);
        let a = names(1);
        let b = names(2);
        set_shuffle_seed(0);

        // Same set either way — a permutation, never a filter.
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        assert_eq!(sorted(plain.clone()), sorted(a.clone()));
        assert_eq!(sorted(plain.clone()), sorted(b.clone()));
        // Two seeds disagree with each other (12 entries: ~1/479M by chance).
        assert_ne!(a, b);
        // And reproducible from the seed alone.
        assert_eq!(a, names(1));
        set_shuffle_seed(0);
    }
}
