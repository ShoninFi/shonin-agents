//! What a report's items say, as the model's eight inputs, and the same
//! placeholder score written down, for when the model can't be run.
//!
//! The model is a placeholder: a fixed linear score of which items a report
//! carries (`sigmoid(features · WEIGHTS + BIAS)`), not trained on anything.
//! The weights are here so a reviewer can read exactly what it does.

use crate::edgar::Report;

/// Results of operations, a material agreement, a departure or appointment
/// of officers, other events, Regulation FD, non-reliance on earlier
/// statements, an impairment, and whether the report came outside the
/// market's hours.
pub const ITEMS: [&str; 7] = ["2.02", "1.01", "5.02", "8.01", "7.01", "4.02", "2.06"];
pub const WEIGHTS: [f32; 8] = [0.8, 0.5, -0.6, 0.3, 0.2, -0.4, 0.1, 0.05];
pub const BIAS: f32 = -0.1;

/// The model's input: one for each item the report carries, then whether it
/// was accepted outside the market's hours (Eastern, 9:30 to 16:00).
pub fn features(report: &Report) -> [f32; 8] {
    let mut features = [0f32; 8];
    for (i, item) in ITEMS.iter().enumerate() {
        if report.items.iter().any(|carried| carried == item) {
            features[i] = 1.0;
        }
    }
    features[7] = if after_hours(&report.accepted) {
        1.0
    } else {
        0.0
    };
    features
}

/// `2026-10-05T16:05:12.000Z`-style times as EDGAR writes them (Eastern).
fn after_hours(accepted: &str) -> bool {
    let Some(time) = accepted.split('T').nth(1) else {
        return false;
    };
    let mut parts = time.split(':');
    let (Some(hour), Some(minute)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(hour), Ok(minute)) = (hour.parse::<u32>(), minute.parse::<u32>()) else {
        return false;
    };
    let minutes = hour * 60 + minute;
    !(570..960).contains(&minutes)
}

/// The placeholder's score, computed here.
pub fn written_down(features: &[f32; 8]) -> f32 {
    let z: f32 = features
        .iter()
        .zip(WEIGHTS)
        .map(|(x, w)| x * w)
        .sum::<f32>()
        + BIAS;
    1.0 / (1.0 + (-z).exp())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(items: &[&str], accepted: &str) -> Report {
        Report {
            accession: "x".into(),
            accepted: accepted.into(),
            items: items.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn results_after_the_close_score_high_and_a_restatement_low() {
        let results = written_down(&features(&report(
            &["2.02", "9.01"],
            "2026-10-05T16:30:00.000Z",
        )));
        assert!(results > 0.6, "{results}");
        let restated = written_down(&features(&report(
            &["4.02", "5.02"],
            "2026-10-05T10:00:00.000Z",
        )));
        assert!(restated < 0.35, "{restated}");
    }

    #[test]
    fn an_unknown_item_scores_nothing_and_the_hours_are_read_as_eastern() {
        let f = features(&report(&["9.01"], "2026-10-05T12:00:00.000Z"));
        assert_eq!(f, [0.0; 8]);
        assert_eq!(features(&report(&[], "2026-10-05T09:29:00.000Z"))[7], 1.0);
        assert_eq!(features(&report(&[], "2026-10-05T09:30:00.000Z"))[7], 0.0);
    }
}
