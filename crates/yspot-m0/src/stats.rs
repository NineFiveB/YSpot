//! Percentiles, same convention as the matcher bench: nearest-rank on a
//! sorted copy, so a p95 over 100 samples is the 95th value, not an
//! interpolation.

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

pub fn stats(mut v: Vec<f64>) -> Stats {
    if v.is_empty() {
        return Stats::default();
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("latency samples are finite"));
    let at = |q: f64| v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
    Stats {
        n: v.len(),
        min: v[0],
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: v[v.len() - 1],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_percentiles() {
        let s = stats((1..=100).map(f64::from).collect());
        assert_eq!(s.n, 100);
        assert_eq!(s.min, 1.0);
        assert_eq!(s.p50, 50.0);
        assert_eq!(s.p95, 95.0);
        assert_eq!(s.p99, 99.0);
        assert_eq!(s.max, 100.0);
    }

    #[test]
    fn single_sample_is_every_percentile() {
        let s = stats(vec![7.5]);
        assert_eq!(s.p50, 7.5);
        assert_eq!(s.p95, 7.5);
        assert_eq!(s.max, 7.5);
    }

    #[test]
    fn empty_is_zeroed_not_a_panic() {
        let s = stats(Vec::new());
        assert_eq!(s.n, 0);
    }
}
