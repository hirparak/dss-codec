use crate::error::{DecodeError, Result};

/// Resample mono f64 audio from one sample rate to another using rubato,
/// without the resampler's delay: output sample n is the input at time
/// n / to_rate.
pub fn resample(
    samples: &[f64],
    from_rate: u32,
    to_rate: u32,
) -> Result<Vec<f64>> {
    if from_rate == to_rate {
        return Ok(samples.to_vec());
    }

    use rubato::{FftFixedInOut, Resampler};

    let chunk_size = 1024;
    let mut resampler = FftFixedInOut::<f64>::new(
        from_rate as usize,
        to_rate as usize,
        chunk_size,
        1, // mono
    )
    .map_err(|e| DecodeError::Resample(e.to_string()))?;
    if !resampler.output_frames_next().is_multiple_of(2) {
        resampler = FftFixedInOut::<f64>::new(
            from_rate as usize,
            to_rate as usize,
            2 * resampler.input_frames_next(),
            1,
        )
        .map_err(|e| DecodeError::Resample(e.to_string()))?;
    }
    let delay = resampler.output_delay();
    let expected_len =
        (samples.len() as f64 * to_rate as f64 / from_rate as f64).round() as usize;

    let mut output = Vec::new();
    let input_frames_needed = resampler.input_frames_next();
    let mut pos = 0;

    while output.len() < delay + expected_len {
        let start = pos.min(samples.len());
        let end = (pos + input_frames_needed).min(samples.len());
        let mut chunk: Vec<f64> = samples[start..end].to_vec();

        // Pad with zeros if needed
        chunk.resize(input_frames_needed, 0.0);

        let result = resampler
            .process(&[chunk], None)
            .map_err(|e| DecodeError::Resample(e.to_string()))?;

        if !result.is_empty() {
            output.extend_from_slice(&result[0]);
        }

        pos += input_frames_needed;
    }

    // Trim to expected length
    output.drain(..delay);
    output.truncate(expected_len);

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, len: usize) -> Vec<f64> {
        (0..len)
            .map(|n| 8000.0 * (2.0 * std::f64::consts::PI * 440.0 * n as f64 / rate as f64).sin())
            .collect()
    }

    #[test]
    fn resample_has_no_delay() {
        for (from, to) in [
            (16000, 12000),
            (12000, 16000),
            (8000, 12000),
            (12000, 11025),
            (12000, 22050),
            (16000, 44100),
        ] {
            let out = resample(&tone(from, 3 * from as usize), from, to).unwrap();
            assert_eq!(out.len(), 3 * to as usize);
            let expected = tone(to, out.len());
            let middle = to as usize..2 * to as usize;
            let worst = middle
                .map(|n| (out[n] - expected[n]).abs())
                .fold(0.0, f64::max);
            assert!(worst < 1.0, "{from} to {to}: {worst}");
        }
    }
}
