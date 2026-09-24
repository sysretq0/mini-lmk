use std::hash::{BuildHasherDefault, Hasher};

/// Minimal, zero-dependency 64-bit FNV-1a hasher.
/// Eliminates SipHash cryptographic overhead for internal hash tables (PID and package lookups).
#[derive(Debug, Clone)]
pub struct FnvHasher(u64);

const FNV_OFFSET_BASIS_64: u64 = 0xcbf29ce484222325;
const FNV_PRIME_64: u64 = 0x100000001b3;

impl Default for FnvHasher {
    #[inline(always)]
    fn default() -> Self {
        FnvHasher(FNV_OFFSET_BASIS_64)
    }
}

impl Hasher for FnvHasher {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= byte as u64;
            self.0 = self.0.wrapping_mul(FNV_PRIME_64);
        }
    }

    #[inline(always)]
    fn write_u32(&mut self, i: u32) {
        self.0 ^= i as u64;
        self.0 = self.0.wrapping_mul(FNV_PRIME_64);
    }

    #[inline(always)]
    fn write_u64(&mut self, i: u64) {
        self.0 ^= i;
        self.0 = self.0.wrapping_mul(FNV_PRIME_64);
    }

    #[inline(always)]
    fn write_usize(&mut self, i: usize) {
        self.0 ^= i as u64;
        self.0 = self.0.wrapping_mul(FNV_PRIME_64);
    }
}

pub type FastHasherBuilder = BuildHasherDefault<FnvHasher>;
pub type FastMap<K, V> = std::collections::HashMap<K, V, FastHasherBuilder>;
pub type FastSet<T> = std::collections::HashSet<T, FastHasherBuilder>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fnv1a_map_operations() {
        let mut map = FastMap::default();
        map.insert("com.google.android.calculator".to_string(), 12763u32);
        map.insert("com.android.chrome".to_string(), 15400u32);

        assert_eq!(map.get("com.google.android.calculator"), Some(&12763));
        assert_eq!(map.get("com.android.chrome"), Some(&15400));
        assert_eq!(map.get("com.nonexistent"), None);

        map.remove("com.google.android.calculator");
        assert_eq!(map.get("com.google.android.calculator"), None);
    }

    #[test]
    fn test_fnv1a_set_operations() {
        let mut set = FastSet::default();
        set.insert(1234u32);
        set.insert(5678u32);

        assert!(set.contains(&1234));
        assert!(set.contains(&5678));
        assert!(!set.contains(&9999));
    }
}

