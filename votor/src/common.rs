use {
    agave_votor_messages::{
        consensus_message::CertificateType,
        fraction::Fraction,
        vote::{Vote, VoteType},
    },
    solana_clock::{NUM_CONSECUTIVE_LEADER_SLOTS, Slot},
    std::{
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    },
};

// Core consensus types and constants
pub type Stake = u64;

pub const fn conflicting_types(vote_type: VoteType) -> &'static [VoteType] {
    match vote_type {
        VoteType::Finalize => &[
            VoteType::NotarizeFallback,
            VoteType::Skip,
            VoteType::SkipFallback,
        ],
        VoteType::Notarize => &[VoteType::Skip, VoteType::NotarizeFallback],
        VoteType::NotarizeFallback => &[VoteType::Finalize, VoteType::Notarize],
        VoteType::Skip => &[
            VoteType::Finalize,
            VoteType::Notarize,
            VoteType::SkipFallback,
        ],
        VoteType::SkipFallback => &[VoteType::Skip, VoteType::Finalize],
        VoteType::Genesis => &[
            VoteType::Finalize,
            VoteType::Notarize,
            VoteType::NotarizeFallback,
            VoteType::Skip,
            VoteType::SkipFallback,
        ],
    }
}

/// Lookup from `Vote` to the `CertificateId`s the vote accounts for
///
/// Must be in sync with `certificate_limits_and_vote_types` and `VoteType::get_type`
pub fn vote_to_cert_types(vote: &Vote) -> Vec<CertificateType> {
    match vote {
        Vote::Notarize(vote) => vec![
            CertificateType::Notarize(vote.slot, vote.block_id),
            CertificateType::NotarizeFallback(vote.slot, vote.block_id),
            CertificateType::FinalizeFast(vote.slot, vote.block_id),
        ],
        Vote::NotarizeFallback(vote) => {
            vec![CertificateType::NotarizeFallback(vote.slot, vote.block_id)]
        }
        Vote::Finalize(vote) => vec![CertificateType::Finalize(vote.slot)],
        Vote::Skip(vote) => vec![CertificateType::Skip(vote.slot)],
        Vote::SkipFallback(vote) => vec![CertificateType::Skip(vote.slot)],
        Vote::Genesis(vote) => vec![CertificateType::Genesis(vote.slot, vote.block_id)],
    }
}

pub const MAX_ENTRIES_PER_PUBKEY_FOR_OTHER_TYPES: usize = 1;
pub const MAX_ENTRIES_PER_PUBKEY_FOR_NOTARIZE_LITE: usize = 3;
pub const MAX_NOTAR_FALLBACK_BLOCKS: usize = 7;

pub const SAFE_TO_NOTAR_MIN_NOTARIZE_ONLY: Fraction = Fraction::from_percentage(40);
pub const SAFE_TO_NOTAR_MIN_NOTARIZE_FOR_NOTARIZE_OR_SKIP: Fraction = Fraction::from_percentage(20);
pub const SAFE_TO_NOTAR_MIN_NOTARIZE_AND_SKIP: Fraction = Fraction::from_percentage(60);

pub const SAFE_TO_SKIP_THRESHOLD: Fraction = Fraction::from_percentage(40);

/// Time bound assumed on network transmission delays during periods of synchrony.
pub const DELTA: Duration = Duration::from_millis(250);

/// Base timeout for when leader's first slice should arrive if they sent it immediately.
pub(crate) const DELTA_TIMEOUT: Duration = DELTA.checked_mul(3).unwrap();

/// Timeout for standstill detection mechanism.
pub(crate) const DELTA_STANDSTILL: Duration = Duration::from_millis(10_000);

/// Maximum standstill timeout extension, capped at 1 hour.
pub const MAX_STANDSTILL_TIMEOUT: Duration = Duration::from_secs(3600);

/// Sentinel value representing "no active standstill" in [`StandstillSignal`].
const NO_STANDSTILL: u64 = u64::MAX;

/// Shared signal for standstill state under unknown network DELTA.
///
/// Holds either no active standstill, or the highest finalized slot at the
/// moment standstill was first detected. Read by every subsystem that needs to
/// scale its network-DELTA-derived timeouts (votor timer manager, repair).
#[derive(Debug)]
pub struct StandstillSignal {
    inner: AtomicU64,
}

impl Default for StandstillSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl StandstillSignal {
    pub fn new() -> Self {
        Self {
            inner: AtomicU64::new(NO_STANDSTILL),
        }
    }

    pub fn get(&self) -> Option<Slot> {
        match self.inner.load(Ordering::Relaxed) {
            NO_STANDSTILL => None,
            slot => Some(slot),
        }
    }

    pub fn set(&self, slot: Slot) {
        self.inner.store(slot, Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.inner.store(NO_STANDSTILL, Ordering::Relaxed);
    }
}

/// Multiplier applied to network-DELTA-derived timeouts under unknown DELTA.
///
/// Returns 1.0 outside standstill. During standstill, returns `1.05^n` where
/// `n` is the number of leader windows elapsed since standstill began. Used
/// to discover the actual network DELTA by exponentially growing every retry
/// timeout — see Alpenglow partial-synchrony recovery.
pub fn calculate_timeout_multiplier(slot: Slot, standstill_slot: Option<Slot>) -> f64 {
    match standstill_slot {
        None => 1.0,
        Some(standstill_slot) => {
            let slots_since_standstill = slot.saturating_sub(standstill_slot);
            let leader_windows = slots_since_standstill / NUM_CONSECUTIVE_LEADER_SLOTS;
            1.05_f64.powi(leader_windows as i32)
        }
    }
}

/// Scale a base timeout by the standstill multiplier and cap at
/// [`MAX_STANDSTILL_TIMEOUT`].
pub fn scale_standstill_timeout(base: Duration, slot: Slot, standstill_slot: Option<Slot>) -> Duration {
    let multiplier = calculate_timeout_multiplier(slot, standstill_slot);
    let scaled = base.as_secs_f64() * multiplier;
    let capped = scaled.min(MAX_STANDSTILL_TIMEOUT.as_secs_f64());
    Duration::from_secs_f64(capped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_timeout_multiplier() {
        // No standstill - multiplier is 1.0.
        assert_eq!(calculate_timeout_multiplier(100, None), 1.0);

        // At standstill_slot itself - 0 leader windows elapsed.
        assert_eq!(calculate_timeout_multiplier(0, Some(0)), 1.0);

        // 4 slots = 1 leader window = 1.05^1.
        let m = calculate_timeout_multiplier(4, Some(0));
        assert!((m - 1.05).abs() < 0.001);

        // 8 slots = 2 leader windows = 1.05^2.
        let m = calculate_timeout_multiplier(8, Some(0));
        assert!((m - 1.1025).abs() < 0.001);

        // 40 slots = 10 leader windows = 1.05^10.
        let m = calculate_timeout_multiplier(40, Some(0));
        assert!((m - 1.05_f64.powi(10)).abs() < 0.001);

        // 8 slots after standstill_slot=20.
        let m = calculate_timeout_multiplier(28, Some(20));
        assert!((m - 1.1025).abs() < 0.001);
    }

    #[test]
    fn test_standstill_signal() {
        let s = StandstillSignal::new();
        assert_eq!(s.get(), None);
        s.set(42);
        assert_eq!(s.get(), Some(42));
        s.set(100);
        assert_eq!(s.get(), Some(100));
        s.clear();
        assert_eq!(s.get(), None);
    }

    #[test]
    fn test_scale_standstill_timeout_caps() {
        let base = DELTA_TIMEOUT;
        // Without standstill, no scaling.
        assert_eq!(scale_standstill_timeout(base, 100, None), base);
        // With absurdly large window count, capped at MAX_STANDSTILL_TIMEOUT.
        let capped = scale_standstill_timeout(base, 4 * 400, Some(0));
        assert_eq!(capped, MAX_STANDSTILL_TIMEOUT);
    }
}
