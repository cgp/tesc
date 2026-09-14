//! Deterministic randomness for generated traffic.
//!
//! Seeded from the run seed, which is recorded in the run's identity (design-engine
//! §7.1), so a run can be replayed with identical generated traffic. Without that,
//! comparing two runs means comparing two different workloads.
//!
//! **Seeded per iteration rather than per virtual user.** A slot is a virtual user,
//! and which slot picks up which arrival depends on how long the service took to
//! answer the arrival before it — so a per-slot stream produces different values on a
//! replay of the same plan against a slightly slower service. Keying the stream to the
//! iteration number instead makes iteration 4,001 generate the same request in every
//! run of the plan, which is the property the seed exists for.
//!
//! SplitMix64: eight bytes of state, one multiply-xor-shift per value, no allocation.
//! It is not a cryptographic generator and nothing here wants one — these values pick
//! rows and fill request fields.

/// One iteration's stream of values.
#[derive(Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    /// The stream for one iteration of one run.
    pub fn seeded(seed: u64, iteration: u64) -> Self {
        // Mixed rather than added: two runs whose seeds differ by one must not
        // produce streams that differ only by one iteration's worth of offset.
        Self(mix(seed ^ iteration.wrapping_mul(0x9e37_79b9_7f4a_7c15)))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix(self.0)
    }

    /// A number in `low..=high`, with `low <= high` guaranteed by the caller.
    pub fn in_range(&mut self, low: i64, high: i64) -> i64 {
        let span = high.wrapping_sub(low) as u64;
        if span == u64::MAX {
            return self.next_u64() as i64;
        }
        // Modulo, whose bias over a span this small is far below anything a load
        // plan can observe, and which costs one instruction rather than a loop.
        low.wrapping_add((self.next_u64() % (span + 1)) as i64)
    }

    /// A version 4 UUID, drawn from this stream rather than from the system.
    pub fn uuid(&mut self) -> String {
        let (high, low) = (self.next_u64(), self.next_u64());
        let time_low = (high >> 32) as u32;
        let time_mid = (high >> 16) as u16;
        let time_high = ((high as u16) & 0x0fff) | 0x4000;
        let clock = ((low >> 48) as u16 & 0x3fff) | 0x8000;
        let node = low & 0xffff_ffff_ffff;
        format!("{time_low:08x}-{time_mid:04x}-{time_high:04x}-{clock:04x}-{node:012x}")
    }
}

/// One value, thoroughly stirred. Also used on its own to derive a seed.
pub(crate) fn mix(value: u64) -> u64 {
    let mut value = value;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_and_iteration_give_the_same_values() {
        let values = |seed, iteration| {
            let mut rng = Rng::seeded(seed, iteration);
            (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        // The whole point of a recorded seed: a replay is the same workload.
        assert_eq!(values(7, 4001), values(7, 4001));
        assert_ne!(values(7, 4001), values(7, 4002));
        assert_ne!(values(7, 4001), values(8, 4001));
    }

    #[test]
    fn a_range_stays_inside_itself_and_covers_it() {
        let mut rng = Rng::seeded(1, 1);
        let mut seen = [false; 20];
        for _ in 0..2000 {
            let value = rng.in_range(1, 20);
            assert!((1..=20).contains(&value), "{value}");
            seen[value as usize - 1] = true;
        }
        // Both ends inclusive: `rand(1, 20)` in a plan means twenty possible values,
        // and a generator that never emitted 20 would be sending a different mix.
        assert!(seen.iter().all(|&hit| hit));
    }

    #[test]
    fn a_single_valued_range_is_that_value() {
        let mut rng = Rng::seeded(1, 1);
        assert_eq!(rng.in_range(5, 5), 5);
    }

    #[test]
    fn a_uuid_has_the_shape_a_uuid_has() {
        let mut rng = Rng::seeded(3, 9);
        let uuid = rng.uuid();
        assert_eq!(uuid.len(), 36);
        let fields: Vec<&str> = uuid.split('-').collect();
        assert_eq!(
            fields.iter().map(|f| f.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(uuid.starts_with(|c: char| c.is_ascii_hexdigit()));
        // Version 4 and the RFC's variant bits, so a service that parses it accepts
        // it. Deterministic and still well-formed.
        assert!(fields[2].starts_with('4'), "{uuid}");
        assert!(
            ["8", "9", "a", "b"]
                .iter()
                .any(|bit| fields[3].starts_with(bit)),
            "{uuid}"
        );
        assert_ne!(uuid, rng.uuid());
    }
}
