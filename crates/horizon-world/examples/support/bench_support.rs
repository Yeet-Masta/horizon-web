//! Shared reporting for the opt-in headless baselines. Call outside timed regions.

fn median(values: &mut [f64]) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    let high = values.len() / 2;
    if values.len().is_multiple_of(2) {
        values[high - 1].midpoint(values[high])
    } else {
        values[high]
    }
}

/// Report samples in run order, their median, and the median absolute deviation.
/// `mad_over_median` is a fraction; a perfectly flat zero-valued control is zero.
pub fn stats(samples: &[f64]) -> serde_json::Value {
    assert!(
        !samples.is_empty(),
        "benchmark must record at least one sample"
    );
    let mut scratch = samples.to_vec();
    let middle = median(&mut scratch);
    for (deviation, &sample) in scratch.iter_mut().zip(samples) {
        *deviation = (sample - middle).abs();
    }
    let mad = median(&mut scratch);
    let ratio = if middle != 0.0 {
        Some(mad / middle.abs())
    } else if mad == 0.0 {
        Some(0.0)
    } else {
        None
    };
    serde_json::json!({
        "samples": samples,
        "median": middle,
        "mad": mad,
        "mad_over_median": ratio,
    })
}
