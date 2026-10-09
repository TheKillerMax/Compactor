//! Pure decisions shared by analysis and the Windows compression worker.
//!
//! An estimate is advisory. Only allocated bytes measured around a WOF operation
//! can satisfy the retention threshold.

pub fn sample_is_candidate(ratio: f32, ratio_limit: f32) -> bool {
    ratio.is_finite()
        && ratio_limit.is_finite()
        && (0.0..1.0).contains(&ratio)
        && (0.0..=1.0).contains(&ratio_limit)
        && ratio <= ratio_limit
}

pub fn savings_worthwhile(
    before: u64,
    after: u64,
    min_savings_percent: f32,
    cluster_size: u64,
) -> bool {
    if before == 0
        || after >= before
        || !min_savings_percent.is_finite()
        || !(0.0..=100.0).contains(&min_savings_percent)
    {
        return false;
    }

    // Round the requirement up: a fraction of a byte is not a saving. f64
    // avoids the much coarser f32 conversion of large file sizes. Always demand
    // at least one allocation unit, even when the percentage is set to zero.
    let minimum_bytes = ((before as f64 * min_savings_percent as f64 / 100.0).ceil() as u64)
        .max(cluster_size.max(1));
    before - after >= minimum_bytes
}

pub fn round_up_to_cluster(size: u64, cluster_size: u64) -> u64 {
    let cluster = cluster_size.max(1);
    let remainder = size % cluster;
    if remainder == 0 {
        size
    } else {
        size.saturating_add(cluster - remainder)
    }
}

/// A generic sample projection, not an algorithm-specific benchmark.
pub fn estimated_allocation(
    logical_size: u64,
    ratio: f32,
    cluster_size: u64,
    current_allocation: u64,
) -> u64 {
    if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
        return current_allocation;
    }
    let sample_bytes = (logical_size as f64 * ratio as f64).ceil() as u64;
    // A nonempty WOF stream needs storage even if the sample is all zeros.
    let minimum = if logical_size == 0 {
        0
    } else {
        cluster_size.max(1)
    };
    round_up_to_cluster(sample_bytes.max(minimum), cluster_size).min(current_allocation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_files_must_save_an_allocation_unit_and_the_requested_percentage() {
        assert!(savings_worthwhile(409_600, 405_504, 1.0, 4096));
        assert!(!savings_worthwhile(409_600, 405_505, 1.0, 4096));
        assert!(!savings_worthwhile(8192, 8191, 0.0, 4096));
        assert!(savings_worthwhile(8192, 4096, 0.0, 4096));
        assert!(!savings_worthwhile(131_072, 126_976, 1.0, 65_536));
        assert!(savings_worthwhile(131_072, 65_536, 50.0, 65_536));
    }

    #[test]
    fn invalid_and_non_beneficial_results_are_never_retained() {
        for threshold in [f32::NAN, f32::INFINITY, -1.0, 101.0] {
            assert!(!savings_worthwhile(8192, 4096, threshold, 4096));
        }
        assert!(!savings_worthwhile(0, 0, 0.0, 4096));
        assert!(!savings_worthwhile(8192, 8192, 0.0, 4096));
        assert!(!savings_worthwhile(8192, 16384, 0.0, 4096));
        assert!(!savings_worthwhile(8192, 4096, 100.0, 4096));
    }

    #[test]
    fn zero_threshold_does_not_admit_negative_or_incompressible_samples() {
        for ratio in [f32::NAN, f32::INFINITY, -0.1, 1.0, 1.1] {
            assert!(!sample_is_candidate(ratio, 1.0));
        }
        assert!(sample_is_candidate(0.5, 0.5));
        assert!(!sample_is_candidate(0.6, 0.5));
        assert!(!sample_is_candidate(0.5, f32::NAN));
    }

    #[test]
    fn allocation_rounding_is_zero_safe_and_never_wraps() {
        assert_eq!(0, round_up_to_cluster(0, 4096));
        assert_eq!(4096, round_up_to_cluster(4096, 4096));
        assert_eq!(8192, round_up_to_cluster(4097, 4096));
        assert_eq!(42, round_up_to_cluster(42, 0));
        assert_eq!(u64::MAX, round_up_to_cluster(u64::MAX, 4096));
    }

    #[test]
    fn generic_estimates_do_not_invent_algorithm_gains_or_zero_cost_files() {
        assert_eq!(
            524_288,
            estimated_allocation(1_048_576, 0.5, 4096, 1_048_576)
        );
        assert_eq!(4096, estimated_allocation(1_048_576, 0.0, 4096, 1_048_576));
        assert_eq!(8192, estimated_allocation(8192, f32::NAN, 4096, 8192));
        assert_eq!(0, estimated_allocation(0, 0.0, 4096, 0));
        assert_eq!(1024, estimated_allocation(1024, 0.5, 4096, 1024));
    }

    #[test]
    fn large_file_thresholds_do_not_overflow() {
        assert!(savings_worthwhile(u64::MAX, u64::MAX / 2, 49.0, 4096));
        assert!(!savings_worthwhile(u64::MAX, u64::MAX / 2, 51.0, 4096));
    }
}
