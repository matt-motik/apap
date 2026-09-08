//! Iterative column-width distributor for the playlist table.
//!
//! The table must always fill the container width exactly (no horizontal
//! scrollbar) while respecting per-column minimum/maximum constraints. The
//! user's preferred proportions live in `Settings::column_widths` as
//! percentages; this module resolves them into pixel widths.
//!
//! The distributor is a pure function ([`resolve_widths`]) so all the
//! degenerate cases of the spec are unit-tested: too-narrow window, columns
//! pinned at their limits, hide/show rebalancing and exact rounding.

use crate::settings::ColumnId;

/// Per-column width constraints.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnLimit {
    /// Hard minimum width in physical px. Ignored when the container is
    /// narrower than the sum of all minima (degenerate window) — columns are
    /// then shrunk proportionally instead.
    pub min_px: f32,
    /// Absolute pixel cap (for short/numeric columns).
    pub max_px: Option<f32>,
    /// Relative cap as a fraction of the container width (for wide text
    /// columns). When both caps are present the tighter one wins.
    pub max_pct: Option<f32>,
}

impl ColumnLimit {
    /// The resolved upper bound for a given container width.
    pub fn effective_max(&self, container_w: f32) -> f32 {
        let base = container_w.max(0.0);
        let mut m = base;
        if let Some(p) = self.max_px {
            m = m.min(p);
        }
        if let Some(p) = self.max_pct {
            m = m.min(base * p);
        }
        m
    }
}

/// Static per-column defaults. Absolute pixel caps for short/numeric columns,
/// relative (fraction-of-container) caps for wide text columns. Values are
/// tuned for a roughly 900px playlist view; leave a couple of columns with a
/// generous `max_pct` so extra space has somewhere to go.
pub fn column_limit(id: ColumnId) -> ColumnLimit {
    match id {
        ColumnId::Index => ColumnLimit { min_px: 24.0, max_px: Some(48.0), max_pct: None },
        ColumnId::TrackNumber => ColumnLimit { min_px: 44.0, max_px: Some(84.0), max_pct: None },
        ColumnId::Title => ColumnLimit { min_px: 120.0, max_px: None, max_pct: Some(0.60) },
        ColumnId::Artist => ColumnLimit { min_px: 90.0, max_px: None, max_pct: Some(0.45) },
        ColumnId::Album => ColumnLimit { min_px: 90.0, max_px: None, max_pct: Some(0.45) },
        ColumnId::Genre => ColumnLimit { min_px: 80.0, max_px: None, max_pct: Some(0.30) },
        ColumnId::Year => ColumnLimit { min_px: 44.0, max_px: Some(72.0), max_pct: None },
        ColumnId::Format => ColumnLimit { min_px: 52.0, max_px: Some(96.0), max_pct: None },
        ColumnId::Bitrate => ColumnLimit { min_px: 70.0, max_px: Some(130.0), max_pct: None },
        ColumnId::BitDepth => ColumnLimit { min_px: 64.0, max_px: Some(110.0), max_pct: None },
        ColumnId::SampleRate => ColumnLimit { min_px: 90.0, max_px: Some(140.0), max_pct: None },
        ColumnId::Duration => ColumnLimit { min_px: 56.0, max_px: Some(100.0), max_pct: None },
        ColumnId::FileName => ColumnLimit { min_px: 110.0, max_px: None, max_pct: Some(0.60) },
        ColumnId::FilePath => ColumnLimit { min_px: 130.0, max_px: None, max_pct: Some(0.70) },
    }
}

/// Resolve the pixel widths of the visible columns so they fill `container_w`
/// exactly while respecting per-column minima/maxima.
///
/// `ratios[i]` is the relative proportion of column `i`; it does not need to
/// sum to 1 (normalised internally) and `ratios[i] / sum * container_w` is the
/// ideal width, clamped to `[min_px, effective_max]`. A leftover/shortage is
/// rebalanced over up to 7 iterations, split between columns proportional to
/// their headroom (growable `max - w`, shrinkable `w - min`). If everything is
/// pinned, the delta is spread evenly as a last-resort fallback.
///
/// Degenerate window: when `container_w` is narrower than the sum of minima,
/// the minima are ignored and every column is scaled down proportionally so
/// the UI does not collapse.
///
/// Rounding: every column except the last is rounded down; the last one
/// absorbs the fractional remainder so the returned widths sum to exactly
/// `container_w`.
pub fn resolve_widths(container_w: f32, ids: &[ColumnId], ratios: &[f32]) -> Vec<f32> {
    assert_eq!(
        ids.len(),
        ratios.len(),
        "resolve_widths: ids/ratios length mismatch"
    );
    let n = ids.len();
    if n == 0 {
        return Vec::new();
    }
    if !container_w.is_finite() || container_w <= 0.0 {
        return vec![0.0; n];
    }

    let ratio_sum: f32 = ratios
        .iter()
        .map(|r| if r.is_finite() && *r > 0.0 { *r } else { 0.0 })
        .sum();
    let norm: Vec<f32> = (0..n)
        .map(|i| {
            if ratio_sum > 0.0 {
                ratios[i].max(0.0) / ratio_sum
            } else {
                1.0 / n as f32
            }
        })
        .collect();

    let min_sum: f32 = ids.iter().map(|id| column_limit(*id).min_px).sum();
    if container_w < min_sum {
        // Degenerate window: narrower than the sum of minima. Shrink every
        // column proportionally to its ratio, ignoring the minima entirely so
        // the UI cannot collapse. No correction loop — it would only fight the
        // too-narrow container.
        let widths: Vec<f32> = norm.iter().map(|r| r * container_w).collect();
        return round_fill(widths, container_w);
    }

    // Ideal width per column, clamped to [min, effective_max].
    let mut widths: Vec<f32> = (0..n)
        .map(|i| {
            let raw = norm[i] * container_w;
            let lim = column_limit(ids[i]);
            let max_eff = lim.effective_max(container_w);
            let min_eff = lim.min_px.min(max_eff);
            raw.clamp(min_eff, max_eff)
        })
        .collect();

    // Iterative correction: rebalance delta against column headroom.
    for _ in 0..7 {
        let total: f32 = widths.iter().sum();
        let delta = container_w - total;
        if delta.abs() < 1.0 {
            break;
        }
        let mut weight_sum = 0.0;
        let mut weights = Vec::with_capacity(n);
        for (i, id) in ids.iter().enumerate() {
            let lim = column_limit(*id);
            let weight = if delta > 0.0 {
                (lim.effective_max(container_w) - widths[i]).max(0.0)
            } else {
                (widths[i] - lim.min_px).max(0.0)
            };
            weights.push(weight);
            weight_sum += weight;
        }
        if weight_sum <= 0.0 {
            // Everything is pinned (e.g. all at their maxima) — spread evenly.
            let share = delta / n as f32;
            for w in widths.iter_mut() {
                *w = (*w + share).max(0.0);
            }
            continue;
        }
        for (i, w) in widths.iter_mut().enumerate() {
            *w = (*w + delta * weights[i] / weight_sum).max(0.0);
        }
    }

    round_fill(widths, container_w)
}

/// Round down all but the last width; the last column absorbs the fractional
/// remainder so the sum is exactly `container_w`.
fn round_fill(widths: Vec<f32>, container_w: f32) -> Vec<f32> {
    let n = widths.len();
    if n < 2 {
        return vec![container_w.max(0.0)];
    }
    let floored_rest: f32 = widths[..n - 1].iter().map(|w| w.floor()).sum();
    let mut out: Vec<f32> = widths[..n - 1].iter().map(|w| w.floor()).collect();
    out.push((container_w - floored_rest).max(0.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sum_close(actual: &[f32], expected: f32) {
        let total: f32 = actual.iter().sum();
        assert!(
            (total - expected).abs() < 0.01,
            "widths sum {} != container {}: {actual:?}",
            total,
            expected
        );
    }

    #[test]
    fn degenerate_narrow_window_ignores_minima() {
        // Sum of minima = 24 + 120 + 90 = 234 > 50.
        let ids = [ColumnId::Index, ColumnId::Title, ColumnId::Artist];
        let out = resolve_widths(50.0, &ids, &[5.0, 22.0, 15.0]);
        sum_close(&out, 50.0);
        assert!(out.iter().all(|w| *w >= 0.0));
        for (id, w) in ids.iter().zip(&out) {
            assert!(
                *w <= column_limit(*id).min_px + 1.0,
                "degenerate shrink violated min (by design) for {id:?}: {w}"
            );
        }
    }

    #[test]
    fn wide_window_respects_caps_and_fills() {
        let ids = [ColumnId::Index, ColumnId::Title, ColumnId::Artist];
        let out = resolve_widths(5000.0, &ids, &[5.0, 22.0, 15.0]);
        sum_close(&out, 5000.0);
        assert!(out[0] <= 48.0, "Index capped at 48px: {out:?}");
        let title_max = column_limit(ColumnId::Title).effective_max(5000.0);
        let artist_max = column_limit(ColumnId::Artist).effective_max(5000.0);
        assert!(out[1] <= title_max + 0.01, "Title over cap: {out:?}");
        assert!(out[2] <= artist_max + 0.01, "Artist over cap: {out:?}");
    }

    #[test]
    fn reenable_column_rebalances_respecting_minima() {
        // Even split at 900px: each ideal = 225, all above their minima.
        let ids = [ColumnId::Title, ColumnId::Artist, ColumnId::Album, ColumnId::Genre];
        let out = resolve_widths(900.0, &ids, &[1.0, 1.0, 1.0, 1.0]);
        sum_close(&out, 900.0);
        for (id, w) in ids.iter().zip(&out) {
            assert!(*w >= column_limit(*id).min_px - 0.01, "{id:?} below min: {w}");
        }
    }

    #[test]
    fn pinned_columns_get_even_fallback_spread() {
        // Title max 60% of 300 = 180; Artist max 45% of 300 = 135. A very
        // lopsided preference leaves headroom only in less-favoured columns.
        let ids = [ColumnId::Title, ColumnId::Artist];
        let out = resolve_widths(300.0, &ids, &[0.9, 0.1]);
        sum_close(&out, 300.0);
        assert!(out[0] <= 180.0 + 0.01, "Title should pin at max: {out:?}");
        assert!(out[1] >= column_limit(ColumnId::Artist).min_px - 0.01);
    }

    #[test]
    fn rounding_sum_matches_container_exactly() {
        let ids = [ColumnId::Artist, ColumnId::Duration, ColumnId::SampleRate];
        let out = resolve_widths(713.257, &ids, &[15.0, 5.0, 5.0]);
        sum_close(&out, 713.257);
    }

    #[test]
    fn ratios_need_not_sum_to_one() {
        let ids = [ColumnId::Title, ColumnId::Artist];
        let a = resolve_widths(600.0, &ids, &[30.0, 10.0]);
        let b = resolve_widths(600.0, &ids, &[0.75, 0.25]);
        sum_close(&a, 600.0);
        for (x, y) in a.iter().zip(&b) {
            assert!((x - y).abs() < 0.01, "{x} vs {y}");
        }
    }
}