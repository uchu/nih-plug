//! The buffer sizes an ASIO driver accepts, from what `ASIOGetBufferSize` reports (spec A12).

/// What `ASIOGetBufferSize` reports for the loaded driver.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BufferFacts {
    pub min: i32,
    pub max: i32,
    pub preferred: i32,
    pub granularity: i32,
}

/// The plug-in's block on a duplex host: its own headroom floor, so no driver period up to it is
/// ever split and no buffer change re-initializes the plug-in (A14).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const DUPLEX_BLOCK: u32 = 8192;

const LISTED: std::ops::RangeInclusive<u32> = 16..=DUPLEX_BLOCK;

/// Every size the driver accepts within 16 to [`DUPLEX_BLOCK`] samples, plus its preferred size
/// wherever that lies. A negative granularity doubles from the minimum, a positive one steps by
/// that many samples, and zero (or a minimum equal to the maximum) admits the preferred size only.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn legal_sizes(facts: BufferFacts) -> anyhow::Result<Vec<u32>> {
    let BufferFacts {
        min,
        max,
        preferred,
        granularity,
    } = facts;
    if preferred <= 0 || min <= 0 || max < min {
        anyhow::bail!(
            "the driver reports unusable buffer sizes (min {min}, max {max}, preferred \
             {preferred}, granularity {granularity})"
        );
    }
    let (min, max, preferred) = (min as u64, max as u64, preferred as u64);
    let mut sizes = Vec::new();
    if granularity < 0 && min < max {
        let mut size = min;
        while size <= max && size <= u64::from(*LISTED.end()) {
            sizes.push(size);
            size *= 2;
        }
    } else if granularity > 0 && min < max {
        let step = granularity as u64;
        let mut size = min;
        while size <= max && size <= u64::from(*LISTED.end()) {
            sizes.push(size);
            size += step;
        }
    }
    let mut legal: Vec<u32> = sizes
        .into_iter()
        .filter_map(|size| u32::try_from(size).ok())
        .filter(|size| LISTED.contains(size))
        .collect();
    legal.push(preferred as u32);
    legal.sort_unstable();
    legal.dedup();
    Ok(legal)
}

/// The legal size nearest `requested`, the larger one on a tie.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn snap(requested: u32, legal: &[u32]) -> u32 {
    legal
        .iter()
        .copied()
        .min_by_key(|&size| (size.abs_diff(requested), std::cmp::Reverse(size)))
        .unwrap_or(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(min: i32, max: i32, preferred: i32, granularity: i32) -> BufferFacts {
        BufferFacts {
            min,
            max,
            preferred,
            granularity,
        }
    }

    #[test]
    fn power_of_two_granularity_doubles_from_the_minimum() {
        assert_eq!(
            legal_sizes(facts(64, 2048, 256, -1)).unwrap(),
            vec![64, 128, 256, 512, 1024, 2048]
        );
    }

    #[test]
    fn a_linear_granularity_steps_from_the_minimum() {
        assert_eq!(
            legal_sizes(facts(96, 480, 192, 96)).unwrap(),
            vec![96, 192, 288, 384, 480]
        );
    }

    #[test]
    fn granularity_zero_or_a_single_size_offers_only_the_preferred_size() {
        assert_eq!(legal_sizes(facts(64, 2048, 256, 0)).unwrap(), vec![256]);
        assert_eq!(legal_sizes(facts(128, 128, 128, -1)).unwrap(), vec![128]);
    }

    #[test]
    fn the_preferred_size_always_belongs_even_off_the_grid() {
        assert_eq!(
            legal_sizes(facts(64, 512, 200, -1)).unwrap(),
            vec![64, 128, 200, 256, 512]
        );
    }

    #[test]
    fn sizes_outside_16_to_8192_are_dropped_except_the_preferred_one() {
        assert_eq!(legal_sizes(facts(4, 32, 8, -1)).unwrap(), vec![8, 16, 32]);
        assert_eq!(
            legal_sizes(facts(4096, 16384, 4096, -1)).unwrap(),
            vec![4096, 8192]
        );
        assert_eq!(
            legal_sizes(facts(16384, 16384, 16384, 0)).unwrap(),
            vec![16384]
        );
    }

    #[test]
    fn an_unusable_report_is_refused_not_panicked_on() {
        assert!(legal_sizes(facts(64, 2048, 0, -1)).is_err());
        assert!(legal_sizes(facts(0, 2048, 256, -1)).is_err());
        assert!(legal_sizes(facts(512, 256, 256, -1)).is_err());
        assert!(legal_sizes(facts(64, 2048, -5, -1)).is_err());
    }

    #[test]
    fn a_huge_linear_range_stays_bounded() {
        let sizes = legal_sizes(facts(1, i32::MAX, 256, 1)).unwrap();
        assert_eq!(*sizes.last().unwrap(), 8192);
        assert_eq!(sizes.first(), Some(&16));
    }

    #[test]
    fn snap_takes_the_nearest_legal_size_ties_to_the_larger() {
        let legal = [64, 128, 256, 512];
        assert_eq!(snap(256, &legal), 256);
        assert_eq!(snap(200, &legal), 256);
        assert_eq!(snap(192, &legal), 256);
        assert_eq!(snap(150, &legal), 128);
        assert_eq!(snap(1, &legal), 64);
        assert_eq!(snap(100_000, &legal), 512);
    }
}
