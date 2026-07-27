use std::time::Duration;

/// A signed-URL transfer must survive a slow-but-progressing link, so the
/// request deadline is derived from the payload size instead of being a single
/// fixed number. A flat 30s total deadline killed every large object on a
/// domestic uplink regardless of connection quality.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Fixed allowance for TLS, signing, redirects and server think time, added on
/// top of the size-derived transfer allowance.
const TRANSFER_BASE: Duration = Duration::from_secs(30);

/// Floor throughput a transfer must sustain before it is treated as stalled.
/// 128 KiB/s is roughly a 1 Mbit/s link, well below any usable connection.
const MIN_THROUGHPUT_BYTES_PER_SEC: u64 = 128 * 1024;

/// Upper bound so a wedged connection cannot hang a sync forever, and the
/// deadline used when the payload size is not known in advance.
const TRANSFER_CAP: Duration = Duration::from_secs(30 * 60);

/// Deadline for one signed-URL request moving `byte_len` bytes. `None` means the
/// size is not known before the request is sent (a full-object download), which
/// falls back to the cap.
pub(super) fn signed_url_transfer_timeout(byte_len: Option<u64>) -> Duration {
    let Some(byte_len) = byte_len else {
        return TRANSFER_CAP;
    };
    let transfer_seconds = byte_len / MIN_THROUGHPUT_BYTES_PER_SEC;
    TRANSFER_BASE
        .saturating_add(Duration::from_secs(transfer_seconds))
        .min(TRANSFER_CAP)
}

pub(super) fn signed_url_connect_timeout() -> Duration {
    CONNECT_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_payloads_keep_the_base_allowance() {
        assert_eq!(signed_url_transfer_timeout(Some(0)), TRANSFER_BASE);
        assert_eq!(signed_url_transfer_timeout(Some(1024)), TRANSFER_BASE);
    }

    #[test]
    fn large_payloads_scale_past_the_old_flat_timeout() {
        // 300 MiB on a 50 Mbit/s uplink needs ~48s; the old flat 30s deadline
        // failed it every time.
        let deadline = signed_url_transfer_timeout(Some(300 * 1024 * 1024));
        assert!(deadline > Duration::from_secs(60));
        assert!(deadline <= TRANSFER_CAP);
    }

    #[test]
    fn absurd_payloads_and_unknown_sizes_clamp_to_the_cap() {
        assert_eq!(signed_url_transfer_timeout(Some(u64::MAX)), TRANSFER_CAP);
        assert_eq!(signed_url_transfer_timeout(None), TRANSFER_CAP);
    }
}
