//! Process behaviour charts (Shewhart, 1920s; the "XmR" chart as Wheeler
//! applies it to business figures): does the latest value belong with the
//! ones before it, or has something changed?
//!
//! The usual range is worked out from the measure's own history, not from a
//! number someone guessed. Two signals, chosen to keep false alarms rare:
//!
//! - the latest value lies outside the usual range;
//! - the last [`RUN`] values all sit on the same side of the usual middle,
//!   which ordinary ups and downs seldom do (a shift).
//!
//! The middle is the median of the history and the range is 3.145 × the
//! median moving range (the median-based XmR limits), so one past spike
//! neither moves nor widens them. When there is enough history, the range
//! is worked out from the values before the last [`RUN`], so a shift in
//! progress cannot redefine what is usual.

/// Values before the latest that set the usual range, at most.
pub const BASELINE: usize = 20;
/// Values before the latest needed before a range is worked out.
pub const MIN_BASELINE: usize = 7;
/// Values in a row on one side of the average that count as a shift.
pub const RUN: usize = 8;
/// For a normal process the median moving range is about 0.954 σ, so
/// 3 σ ≈ 3.145 median moving ranges.
const LIMIT_FACTOR: f64 = 3.145;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Signal {
    /// Within the usual range and no shift.
    None,
    /// The latest value is above or below the usual range.
    Outside,
    /// The last [`RUN`] values are all above (`true`) or below the middle.
    Run { above: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    pub latest: f64,
    /// The usual middle: the median of the history.
    pub middle: f64,
    pub lower: f64,
    pub upper: f64,
    /// How many earlier values the range was worked out from.
    pub baseline: usize,
    pub signal: Signal,
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// Judges the last of `values` (oldest first) against the ones before it.
/// `None` until there are at least [`MIN_BASELINE`] earlier values.
pub fn judge(values: &[f64]) -> Option<Verdict> {
    let (&latest, earlier) = values.split_last()?;
    if earlier.len() < MIN_BASELINE || !values.iter().all(|v| v.is_finite()) {
        return None;
    }
    // With enough history, judge the last RUN values against what came
    // before them; otherwise against everything before the latest, and
    // look only for values outside the range.
    let shifts = earlier.len() >= MIN_BASELINE + RUN;
    let before = if shifts { &values[..values.len() - RUN] } else { earlier };
    let baseline = &before[before.len().saturating_sub(BASELINE)..];
    let middle = median(&mut baseline.to_vec());
    let mut ranges: Vec<f64> = baseline.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let spread = LIMIT_FACTOR * median(&mut ranges);
    let (lower, upper) = (middle - spread, middle + spread);
    let recent = &values[values.len() - RUN.min(values.len())..];
    let signal = if latest > upper || latest < lower {
        Signal::Outside
    } else if shifts && recent.iter().all(|v| *v > middle) {
        Signal::Run { above: true }
    } else if shifts && recent.iter().all(|v| *v < middle) {
        Signal::Run { above: false }
    } else {
        Signal::None
    };
    Some(Verdict {
        latest,
        middle,
        lower,
        upper,
        baseline: baseline.len(),
        signal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDINARY: [f64; 12] = [
        100.0, 104.0, 97.0, 101.0, 99.0, 103.0, 98.0, 102.0, 100.0, 96.0, 104.0, 99.0,
    ];

    #[test]
    fn ordinary_ups_and_downs_raise_nothing() {
        let verdict = judge(&ORDINARY).unwrap();
        assert_eq!(verdict.signal, Signal::None);
        assert!(verdict.lower < 96.0 && verdict.upper > 104.0, "{verdict:?}");
        assert_eq!(verdict.baseline, 11);
    }

    #[test]
    fn a_value_far_outside_the_usual_range_is_flagged() {
        let mut values = ORDINARY.to_vec();
        values.push(140.0);
        assert_eq!(judge(&values).unwrap().signal, Signal::Outside);
        values.pop();
        values.push(60.0);
        assert_eq!(judge(&values).unwrap().signal, Signal::Outside);
    }

    #[test]
    fn a_quiet_shift_is_flagged_even_inside_the_range() {
        // Twelve ordinary values, then eight a little higher: none of them
        // outside the range, all of them above the average.
        let mut values = ORDINARY.to_vec();
        values.extend([103.0, 104.0, 103.0, 105.0, 104.0, 103.0, 104.0, 105.0]);
        let verdict = judge(&values).unwrap();
        assert!(verdict.latest < verdict.upper, "{verdict:?}");
        assert_eq!(verdict.signal, Signal::Run { above: true });
    }

    #[test]
    fn one_old_spike_does_not_widen_the_range_for_good() {
        let mut values = ORDINARY.to_vec();
        values[3] = 400.0;
        values.push(130.0);
        // With the average moving range the spike would stretch the limits
        // past 130; the median ignores it.
        assert_eq!(judge(&values).unwrap().signal, Signal::Outside);
    }

    #[test]
    fn too_little_history_gives_no_verdict() {
        assert!(judge(&ORDINARY[..MIN_BASELINE]).is_none());
        assert!(judge(&ORDINARY[..=MIN_BASELINE]).is_some());
        assert!(judge(&[]).is_none());
        assert!(judge(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, f64::NAN]).is_none());
    }

    #[test]
    fn only_the_recent_history_sets_the_range() {
        // An old era at 10, then values around 100.
        let mut values = vec![10.0; 30];
        values.extend(ORDINARY.iter().chain(ORDINARY.iter()).chain(ORDINARY[..4].iter()));
        values.push(101.0);
        let verdict = judge(&values).unwrap();
        assert_eq!(verdict.baseline, BASELINE);
        assert!((95.0..105.0).contains(&verdict.middle), "{verdict:?}");
        assert_eq!(verdict.signal, Signal::None);
    }

    #[test]
    fn shifts_are_looked_for_only_with_history_beyond_the_run() {
        // Seven ordinary values then eight higher: not enough history to
        // tell a shift from the start of the series.
        let mut values = ORDINARY[..7].to_vec();
        values.extend([103.0; 8]);
        assert_eq!(judge(&values).unwrap().signal, Signal::None);
    }
}
