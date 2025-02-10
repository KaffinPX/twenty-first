use crate::math::digest::Digest;
use crate::prelude::Tip5;

/// Get a root commitment to the entire MMR/list of Merkle trees
// Follows the description on
// https://github.com/mimblewimble/grin/blob/master/doc/mmr.md#hashing-and-bagging
// to calculate a root from a list of peaks and the size of the MMR. Note, however,
// that the node count described on that website is not used here, as we don't need
// the extra bits of security that that would provide.
pub fn bag_peaks(peaks: &[Digest]) -> Digest {
    let mut peaks = peaks.iter().rev();
    let Some(&last_peak) = peaks.next() else {
        return Tip5::hash(&0u128);
    };

    peaks.fold(last_peak, |acc, &peak| Tip5::hash_pair(peak, acc))
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use rand::prelude::*;

    use super::*;

    #[test]
    fn bag_peaks_snapshot() {
        let mut rng = StdRng::seed_from_u64(0x92ca758afeec6d29);

        assert_snapshot!(bag_peaks(&[]).0[0], @"14353333629925222640");
        assert_snapshot!(bag_peaks(&[rng.random()]).0[0], @"17149516008269095361");
        assert_snapshot!(bag_peaks(&[rng.random(), rng.random()]).0[0], @"06487327802841213551");

        let peaks: [Digest; 10] = rng.random();
        assert_snapshot!(bag_peaks(&peaks).0[0], @"08165051011961773585");
    }
}
