//! Settings that arrive in environment variables, shared with the scripts that start a VM.

const GIB_SUFFIX: char = 'G';
const DEFAULT_GUEST_RAM_GIB: u64 = 16;

/// Reads a size like `16G`. Anything else is `None`.
fn gibibytes(text: &str) -> Option<u64> {
    let digits = text.strip_suffix(GIB_SUFFIX)?;
    let all_digits = !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit());
    all_digits.then(|| digits.parse::<u64>().ok()).flatten()?.checked_mul(1 << 30)
}

/// The guest's RAM, from `GUESTHOUSE_RAM`.
pub fn guest_ram(value: Option<&str>) -> u64 {
    value.and_then(gibibytes).unwrap_or(DEFAULT_GUEST_RAM_GIB << 30)
}

/// The most video memory the guest may have, from `GUESTHOUSE_VRAM`. Zero and unset mean no cap.
pub fn video_cap(value: Option<&str>) -> Option<u64> {
    value.and_then(gibibytes).filter(|&bytes| bytes != 0)
}

/// The guest's video memory cap: the person's own, and never more than the card itself allows.
pub fn within_the_cards(asked: Option<u64>, card: Option<u64>) -> Option<u64> {
    match (asked, card) {
        (Some(asked), Some(card)) => Some(asked.min(card)),
        (asked, card) => asked.or(card),
    }
}

/// Whether the guest keeps a locked allocation's CPU view until the allocation goes, from
/// `GUESTHOUSE_UNMAP_AT_UNLOCK`. Only `1` turns it off, so the guest takes the view away at each
/// last unlock as it did before.
pub fn keep_cpu_views(value: Option<&str>) -> bool {
    value != Some("1")
}

/// The idle cap in microseconds, from `GUESTHOUSE_CPU_IDLE_US`. Unset means no cap and the
/// host keeps its deep sleep states; a number asks the host to wake any core within that
/// many microseconds for as long as a machine runs. Zero is a number too: it leaves the
/// cores no sleep state at all. The node takes a signed 32-bit number and drops a negative
/// one without a word, so a value of 2^31 or above is no number rather than a cap that
/// quietly does nothing.
pub fn idle_cap_us(value: Option<&str>) -> Option<i32> {
    let micros: i32 = value?.parse().ok()?;
    (micros >= -1).then_some(micros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cards_own_cap_holds_whatever_a_person_asks() {
        assert_eq!(within_the_cards(None, None), None, "as before for a card that sets none");
        assert_eq!(within_the_cards(Some(8 << 30), None), Some(8 << 30));
        assert_eq!(within_the_cards(None, Some(6 << 30)), Some(6 << 30));
        assert_eq!(within_the_cards(Some(8 << 30), Some(6 << 30)), Some(6 << 30));
        assert_eq!(within_the_cards(Some(4 << 30), Some(6 << 30)), Some(4 << 30));
    }

    #[test]
    fn ram_defaults_when_unset_or_unreadable() {
        assert_eq!(guest_ram(None), 16 << 30);
        assert_eq!(guest_ram(Some("lots")), 16 << 30);
        assert_eq!(guest_ram(Some("32G")), 32 << 30);
    }

    #[test]
    fn a_cap_needs_a_size() {
        assert_eq!(video_cap(None), None);
        assert_eq!(video_cap(Some("0G")), None);
        assert_eq!(video_cap(Some("G")), None);
        assert_eq!(video_cap(Some("8G")), Some(8 << 30));
    }

    #[test]
    fn cpu_views_are_kept_unless_the_switch_is_one() {
        assert!(keep_cpu_views(None));
        assert!(keep_cpu_views(Some("0")));
        assert!(keep_cpu_views(Some("yes")));
        assert!(!keep_cpu_views(Some("1")));
    }

    /// The node reads a signed 32-bit number and drops a negative one silently, apart from its own
    /// -1 for no constraint, so a value the kernel would turn away is refused here, where it can
    /// be said.
    #[test]
    fn a_cap_the_node_would_read_as_negative_is_no_cap() {
        assert_eq!(idle_cap_us(Some("2147483648")), None);
        assert_eq!(idle_cap_us(Some("4294967295")), None);
        assert_eq!(idle_cap_us(Some("-2")), None);
        assert_eq!(idle_cap_us(Some("")), None);
        // The one negative the node takes is its own "no constraint", so a caller can let go.
        assert_eq!(idle_cap_us(Some("-1")), Some(-1));
    }

    #[test]
    fn an_idle_cap_is_a_number_or_nothing() {
        assert_eq!(idle_cap_us(None), None);
        assert_eq!(idle_cap_us(Some("300")), Some(300));
        assert_eq!(idle_cap_us(Some("0")), Some(0));
        assert_eq!(idle_cap_us(Some("deep")), None);
        assert_eq!(idle_cap_us(Some("2147483647")), Some(i32::MAX));
    }
}
