//! Volume Control and Soft Limiting
//!
//! Port of Go volume processing functions from main_linux.go
//! Provides hybrid OS/DSP volume splitting and soft-clipping

/// Split hybrid volume into OS scalar and DSP boost components.
///
/// For linear volume (0.0-1.0): OS handles directly
/// For boosted volume (> 1.0): OS at max, DSP amplifies
///
/// # Arguments
/// * `slider` - Volume value (0.0 = mute, 1.0 = unity, >1.0 = boost)
///
/// # Returns
/// * `os_scalar` - Volume for OS mixer (0.0-1.0)
/// * `dsp_boost` - Additional gain for DSP pipeline
pub fn split_hybrid_volume(slider: f32) -> (f32, f32) {
    if slider <= 0.001 {
        (0.0, 0.0)
    } else if slider <= 1.0 {
        (slider, 1.0)
    } else {
        (1.0, 1.0 + (slider - 1.0) * 2.0)
    }
}

/// Compute effective total gain from slider position.
///
/// # Arguments
/// * `slider` - Volume value (0.0-3.0 typical range)
///
/// # Returns
/// Total gain multiplier (0.0 = mute, 1.0 = unity, >1.0 = boost)
pub fn effective_total_gain(slider: f32) -> f32 {
    if slider <= 0.001 {
        0.0
    } else if slider <= 1.0 {
        slider
    } else {
        1.0 + (slider - 1.0) * 2.0
    }
}

/// Soft limiter for preventing audio clipping.
///
/// Passes samples through up to |x| = 0.99, then joins a quadratic knee with
/// matching slope. The knee reaches full scale with zero slope at |x| = 1.01.
/// Output is continuous, monotone, odd, and bounded. NaN is silenced for safety.
///
/// # Arguments
/// * `x` - Input sample value
///
/// # Returns
/// Limited output sample
pub fn soft_limit(x: f32) -> f32 {
    if x.is_nan() {
        return 0.0;
    }
    const KNEE: f32 = 0.99;
    const HEADROOM: f32 = 1.0 - KNEE;
    let magnitude = x.abs();
    if magnitude <= KNEE {
        return x;
    }
    if magnitude >= KNEE + 2.0 * HEADROOM {
        return 1.0_f32.copysign(x);
    }
    let excess = magnitude - KNEE;
    (KNEE + excess * (1.0 - excess / (4.0 * HEADROOM))).copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_hybrid_volume_mute() {
        let (os, dsp) = split_hybrid_volume(0.0);
        assert_eq!(os, 0.0);
        assert_eq!(dsp, 0.0);
    }

    #[test]
    fn test_split_hybrid_volume_quiet() {
        let (os, dsp) = split_hybrid_volume(0.0005);
        assert_eq!(os, 0.0);
        assert_eq!(dsp, 0.0);
    }

    #[test]
    fn test_split_hybrid_volume_linear() {
        let (os, dsp) = split_hybrid_volume(0.5);
        assert!((os - 0.5).abs() < 1e-6);
        assert!((dsp - 1.0).abs() < 1e-6);

        let (os, dsp) = split_hybrid_volume(0.75);
        assert!((os - 0.75).abs() < 1e-6);
        assert!((dsp - 1.0).abs() < 1e-6);

        let (os, dsp) = split_hybrid_volume(1.0);
        assert!((os - 1.0).abs() < 1e-6);
        assert!((dsp - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_split_hybrid_volume_boost() {
        let (os, dsp) = split_hybrid_volume(1.5);
        assert!((os - 1.0).abs() < 1e-6);
        assert!((dsp - 2.0).abs() < 1e-6);

        let (os, dsp) = split_hybrid_volume(2.0);
        assert!((os - 1.0).abs() < 1e-6);
        assert!((dsp - 3.0).abs() < 1e-6);
    }

    #[test]
    fn test_effective_total_gain_mute() {
        assert_eq!(effective_total_gain(0.0), 0.0);
        assert_eq!(effective_total_gain(0.0005), 0.0);
    }

    #[test]
    fn test_effective_total_gain_linear() {
        assert!((effective_total_gain(0.5) - 0.5).abs() < 1e-6);
        assert!((effective_total_gain(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_effective_total_gain_boost() {
        assert!((effective_total_gain(1.5) - 2.0).abs() < 1e-6);
        assert!((effective_total_gain(2.0) - 3.0).abs() < 1e-6);
    }

    #[test]
    fn test_soft_limit_pass_through() {
        // Signals within threshold should pass unchanged
        assert!((soft_limit(0.5) - 0.5).abs() < 1e-6);
        assert!((soft_limit(-0.5) - (-0.5)).abs() < 1e-6);
        assert!((soft_limit(0.0) - 0.0).abs() < 1e-6);
        assert!((soft_limit(0.98) - 0.98).abs() < 1e-6);
    }

    #[test]
    fn test_soft_limit_hard_clip() {
        // Extreme values should hard clip
        assert_eq!(soft_limit(100.0), 1.0);
        assert_eq!(soft_limit(-100.0), -1.0);
    }

    #[test]
    fn test_soft_limit_soft_clip() {
        // Values just outside threshold should soft clip
        let result = soft_limit(4.0);
        assert!(result >= -1.0);
        assert!(result <= 1.0);

        let result = soft_limit(-4.0);
        assert!(result >= -1.0);
        assert!(result <= 1.0);
    }

    #[test]
    fn test_soft_limit_symmetry() {
        let pos = soft_limit(2.0);
        let neg = soft_limit(-2.0);
        assert!((pos + neg).abs() < 0.01); // Near-symmetric
    }

    #[test]
    fn limiter_is_continuous_at_both_knee_boundaries() {
        for boundary in [0.99_f32, 1.01_f32] {
            let below = f32::from_bits(boundary.to_bits() - 1);
            let above = f32::from_bits(boundary.to_bits() + 1);
            assert!((soft_limit(above) - soft_limit(below)).abs() < 3e-7);
            assert_eq!(soft_limit(-boundary), -soft_limit(boundary));
        }
        assert_eq!(soft_limit(0.99), 0.99);
        assert_eq!(soft_limit(1.01), 1.0);
    }

    #[test]
    fn limiter_matches_slopes_at_both_knee_boundaries() {
        let delta = 0.0001;
        let slope_at_start = (soft_limit(0.99 + delta) - soft_limit(0.99)) / delta;
        let slope_at_end = (soft_limit(1.01) - soft_limit(1.01 - delta)) / delta;
        assert!((slope_at_start - 1.0).abs() < 0.01);
        assert!(slope_at_end.abs() < 0.01);
    }

    #[test]
    fn limiter_is_globally_monotone_and_bounded() {
        let mut previous = soft_limit(-4.0);
        for i in 1..=80_000 {
            let x = -4.0 + i as f32 * 0.0001;
            let value = soft_limit(x);
            assert!(value >= previous);
            assert!((-1.0..=1.0).contains(&value));
            previous = value;
        }
    }

    #[test]
    fn limiter_joins_knee_without_legacy_downward_jump() {
        assert!(soft_limit(1.0) > soft_limit(0.99));
        assert!((soft_limit(1.0) - 0.9975).abs() < 1e-6);
        assert_eq!(soft_limit(2.0), 1.0);
    }

    #[test]
    fn limiter_silences_nan_and_clips_infinities() {
        assert_eq!(soft_limit(f32::NAN), 0.0);
        assert_eq!(soft_limit(f32::INFINITY), 1.0);
        assert_eq!(soft_limit(f32::NEG_INFINITY), -1.0);
    }
}
