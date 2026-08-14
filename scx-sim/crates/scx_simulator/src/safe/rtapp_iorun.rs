//! The frozen calibrated profile for rt-app's `iorun` verb.
//!
//! # Why this is a second profile and not a widening of the first
//!
//! I/O model v1/v2 calibrate ktstr's `IoSyncWrite`: 4 KiB `O_SYNC` `pwrite`
//! plus `fdatasync` to a raw block device at queue depth one. rt-app's `iorun`
//! is declared in the *same unit* — bytes — and has *opposite physics*: a
//! buffered, unsynced `write(2)` loop whose device defaults to `/dev/null`. It
//! never enters D-state.
//!
//! Feeding rt-app bytes through v1's coefficients was measured, blind, on 70
//! held-out runs: it scores **0/70** with a median absolute relative error of
//! 250,844% (system CPU alone) or 773,461% (v1's implied elapsed total). The
//! best *byte-count* model obtainable from this profile's own training data
//! also fails, 10/70 at a median 81.0% error — so the failure is structural,
//! not a matter of poor coefficients. Evidence:
//! `experiments/io_model_rtapp_iorun_20260814/`.
//!
//! # What drives the cost
//!
//! The **number of `write(2)` calls**, not the byte count. `ioload()` in
//! rt-app's `src/rt-app.c` is
//! `while (count) { write(fd, ptr, min(count, iomem->size)); count -= ret; }`
//! with `iomem->size` being `global.mem_buffer_size`, and `/dev/null`'s write
//! handler consumes the iterator without copying, so cost is `O(1)` in
//! transfer size. Holding the call count fixed and sweeping transfer size 256×
//! moved cost per call by ±2.3%; holding *declared bytes* fixed and sweeping
//! the buffer moved total cost 215×.
//!
//! **`mem_buffer_size` is therefore a required condition, not a hint**:
//! declared bytes alone does not determine the answer.
//!
//! # The `INT_MAX` saturation, and why this refuses rather than models it
//!
//! rt-app's `event_data_t.count` is a 32-bit signed `int` filled by
//! `json_object_get_int`, which clamps. Any `iorun` above
//! [`RTAPP_IORUN_MAX_DECLARED_BYTES`] is silently executed as that value —
//! confirmed by `strace` write counts matching the saturated prediction, not
//! the naive one. This profile **refuses** above the threshold rather than
//! reproducing the clamp, because above it the behaviour is a property of the
//! JSON library's saturation semantics rather than of rt-app.

use std::fmt;

/// Frozen profile identity.
pub const RTAPP_IORUN_PROFILE_ID: &str = "rtapp-iorun-devnull-v1";

/// SHA-256 of `freeze_manifest.json`, which fixes the preregistration, the
/// manifest, the training table, the model, all 70 held-out predictions and
/// the evaluator. Frozen before any held-out response was read.
pub const RTAPP_IORUN_MANIFEST_SHA256: &str =
    "75e92c3ef982b083bf38129d134a6028dd044199341756270859227fb1dc6341";

/// The only `io_device` this profile is calibrated for.
pub const RTAPP_IORUN_DEVICE: &str = "/dev/null";

/// rt-app's own default `io_device` (`parse_global`), applied when a config
/// does not set one.
pub const RTAPP_IORUN_DEFAULT_DEVICE: &str = "/dev/null";

/// rt-app's own default `mem_buffer_size` (`DEFAULT_MEM_BUF_SIZE`), 4 MiB.
///
/// Worth knowing before reading a config: at this default an `"iorun":
/// 8388608` is **two** `write` calls costing about a microsecond, not "8 MiB
/// of I/O".
pub const RTAPP_IORUN_DEFAULT_MEM_BUFFER_SIZE: u64 = 4 * 1024 * 1024;

/// `event_data_t.count` is a 32-bit signed `int`; declared bytes saturate here.
pub const RTAPP_IORUN_MAX_DECLARED_BYTES: u64 = 2_147_483_647;

pub const RTAPP_IORUN_MIN_CALLS: u64 = 512;
pub const RTAPP_IORUN_MAX_CALLS: u64 = 65_536;
pub const RTAPP_IORUN_MIN_MEM_BUFFER_SIZE: u64 = 4_096;
pub const RTAPP_IORUN_MAX_MEM_BUFFER_SIZE: u64 = 1_048_576;

/// Calibrated cost of one `write(2)` call, as an exact rational nanosecond
/// count: `218625 / 2048` ns = 106.75048828125 ns.
///
/// Fitted on 60 training runs across six cells spanning a 128× call range and
/// a 256× buffer range, by median ratio with the intercept restricted to zero.
/// Leave-one-training-cell-out worst-case cell error was 0.98%.
pub const RTAPP_IORUN_NS_PER_CALL_NUM: u128 = 218_625;
pub const RTAPP_IORUN_NS_PER_CALL_DEN: u128 = 2_048;

/// Why a declaration cannot use this profile. Every variant is a refusal, and
/// refusal is the designed behaviour: a wrongly-modelled event does not fail
/// visibly, whereas this does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoRunRefusal {
    /// A real device reintroduces blocking this profile is not calibrated for.
    UnsupportedDevice {
        io_device: String,
    },
    ZeroDeclaredBytes,
    /// Above rt-app's own 32-bit saturation point.
    DeclaredBytesSaturate {
        declared_bytes: u64,
    },
    MemBufferSizeOutsideDomain {
        mem_buffer_size: u64,
    },
    CallsOutsideDomain {
        calls: u64,
    },
}

impl fmt::Display for IoRunRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDevice { io_device } => write!(
                f,
                "`{RTAPP_IORUN_PROFILE_ID}` is calibrated only for io_device \
                 `{RTAPP_IORUN_DEVICE}`, but this config declares `{io_device}`. A real \
                 device reintroduces blocking the profile was not calibrated on; refusing \
                 rather than reusing the coefficient"
            ),
            Self::ZeroDeclaredBytes => write!(f, "iorun declares zero bytes"),
            Self::DeclaredBytesSaturate { declared_bytes } => write!(
                f,
                "iorun declares {declared_bytes} bytes, above rt-app's own 32-bit \
                 saturation point {RTAPP_IORUN_MAX_DECLARED_BYTES}. rt-app would silently \
                 execute {RTAPP_IORUN_MAX_DECLARED_BYTES} instead; refusing rather than \
                 reproducing a JSON-library clamp"
            ),
            Self::MemBufferSizeOutsideDomain { mem_buffer_size } => write!(
                f,
                "global.mem_buffer_size {mem_buffer_size} is outside the calibrated domain \
                 [{RTAPP_IORUN_MIN_MEM_BUFFER_SIZE}, {RTAPP_IORUN_MAX_MEM_BUFFER_SIZE}]. It \
                 is a required condition of this profile, not a hint: it sets the write \
                 call count, and declared bytes alone does not determine the answer"
            ),
            Self::CallsOutsideDomain { calls } => write!(
                f,
                "this declaration resolves to {calls} write() calls, outside the calibrated \
                 domain [{RTAPP_IORUN_MIN_CALLS}, {RTAPP_IORUN_MAX_CALLS}]"
            ),
        }
    }
}

/// The two rt-app globals this profile requires, resolved to rt-app's own
/// defaults when a config omits them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoRunGlobals {
    pub io_device: String,
    pub mem_buffer_size: u64,
}

impl Default for IoRunGlobals {
    fn default() -> Self {
        Self {
            io_device: RTAPP_IORUN_DEFAULT_DEVICE.to_string(),
            mem_buffer_size: RTAPP_IORUN_DEFAULT_MEM_BUFFER_SIZE,
        }
    }
}

/// One `iorun` declaration resolved against the frozen profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIoRun {
    pub profile_id: &'static str,
    pub calibration_manifest_sha256: &'static str,
    pub declared_bytes: u64,
    pub mem_buffer_size: u64,
    /// Derived from the declaration by rt-app's own loop, not fitted.
    pub write_calls: u64,
    /// The single `Phase::SystemCpu` interval this declaration lowers to.
    pub system_cpu_ns: u64,
}

/// Write calls rt-app will actually perform, including its `INT_MAX` clamp.
///
/// Exact, derived from `ioload()`; the only fitted quantity in this profile is
/// the per-call cost.
#[must_use]
pub fn write_calls(declared_bytes: u64, mem_buffer_size: u64) -> u64 {
    if mem_buffer_size == 0 {
        return 0;
    }
    let effective = declared_bytes.min(RTAPP_IORUN_MAX_DECLARED_BYTES);
    effective.div_ceil(mem_buffer_size)
}

/// Resolve one `iorun` declaration, or refuse it.
///
/// Every condition is rechecked here, so constructing [`ResolvedIoRun`]
/// through this function is the only way to obtain a lowering.
pub fn resolve(declared_bytes: u64, globals: &IoRunGlobals) -> Result<ResolvedIoRun, IoRunRefusal> {
    if globals.io_device != RTAPP_IORUN_DEVICE {
        return Err(IoRunRefusal::UnsupportedDevice {
            io_device: globals.io_device.clone(),
        });
    }
    if declared_bytes == 0 {
        return Err(IoRunRefusal::ZeroDeclaredBytes);
    }
    if declared_bytes > RTAPP_IORUN_MAX_DECLARED_BYTES {
        return Err(IoRunRefusal::DeclaredBytesSaturate { declared_bytes });
    }
    if !(RTAPP_IORUN_MIN_MEM_BUFFER_SIZE..=RTAPP_IORUN_MAX_MEM_BUFFER_SIZE)
        .contains(&globals.mem_buffer_size)
    {
        return Err(IoRunRefusal::MemBufferSizeOutsideDomain {
            mem_buffer_size: globals.mem_buffer_size,
        });
    }
    let calls = write_calls(declared_bytes, globals.mem_buffer_size);
    if !(RTAPP_IORUN_MIN_CALLS..=RTAPP_IORUN_MAX_CALLS).contains(&calls) {
        return Err(IoRunRefusal::CallsOutsideDomain { calls });
    }
    Ok(ResolvedIoRun {
        profile_id: RTAPP_IORUN_PROFILE_ID,
        calibration_manifest_sha256: RTAPP_IORUN_MANIFEST_SHA256,
        declared_bytes,
        mem_buffer_size: globals.mem_buffer_size,
        write_calls: calls,
        system_cpu_ns: system_cpu_ns(calls),
    })
}

/// `round_half_up(218625 * calls / 2048)`, ties toward positive infinity —
/// the same numerical convention I/O model v1 froze.
#[must_use]
pub fn system_cpu_ns(calls: u64) -> u64 {
    let numerator = RTAPP_IORUN_NS_PER_CALL_NUM * u128::from(calls);
    let value = (numerator + RTAPP_IORUN_NS_PER_CALL_DEN / 2) / RTAPP_IORUN_NS_PER_CALL_DEN;
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write-call law is rt-app's own loop, and these anchors were
    /// confirmed with `strace -c -e trace=write`, not derived from the model.
    #[test]
    fn write_call_law_matches_the_straced_anchors() {
        assert_eq!(write_calls(33_554_432, 4096), 8192);
        assert_eq!(write_calls(2_147_483_648, 262_144), 8192);
        // Above INT_MAX rt-app clamps: the naive answers would be 8192 both times.
        assert_eq!(write_calls(8_589_934_592, 1_048_576), 2048);
        assert_eq!(write_calls(34_359_738_368, 4_194_304), 512);
        // Partial final write is a whole call.
        assert_eq!(write_calls(100_000_000, 65_536), 1526);
        assert_eq!(write_calls(786_000_000, 524_288), 1500);
    }

    /// Pins the fitted coefficient against the frozen held-out predictions.
    /// These are the exact values in `predictions.csv`, which was committed
    /// before any held-out response was read.
    #[test]
    fn system_cpu_matches_the_frozen_heldout_predictions() {
        assert_eq!(system_cpu_ns(1024), 109_313); // H1
        assert_eq!(system_cpu_ns(16384), 1_749_000); // H2
        assert_eq!(system_cpu_ns(2048), 218_625); // H3
        assert_eq!(system_cpu_ns(32768), 3_498_000); // H4
        assert_eq!(system_cpu_ns(4096), 437_250); // H5
        assert_eq!(system_cpu_ns(1500), 160_126); // H6
        assert_eq!(system_cpu_ns(1526), 162_901); // H7
    }

    #[test]
    fn resolves_inside_the_domain() {
        let g = IoRunGlobals {
            io_device: "/dev/null".into(),
            mem_buffer_size: 16_384,
        };
        let r = resolve(33_554_432, &g).expect("in domain");
        assert_eq!(r.write_calls, 2048);
        assert_eq!(r.system_cpu_ns, 218_625);
        assert_eq!(r.profile_id, RTAPP_IORUN_PROFILE_ID);
    }

    /// The same declared byte count with a different buffer must resolve to a
    /// different answer. This is the whole reason the profile exists, and the
    /// held-out cells H3/H4 are the measured form of it.
    #[test]
    fn identical_bytes_with_a_different_buffer_resolve_differently() {
        let bytes = 1_073_741_824;
        let a = resolve(
            bytes,
            &IoRunGlobals {
                io_device: "/dev/null".into(),
                mem_buffer_size: 262_144,
            },
        )
        .unwrap();
        let b = resolve(
            bytes,
            &IoRunGlobals {
                io_device: "/dev/null".into(),
                mem_buffer_size: 32_768,
            },
        )
        .unwrap();
        assert_eq!(a.declared_bytes, b.declared_bytes);
        assert_eq!(a.write_calls * 8, b.write_calls);
        assert!(b.system_cpu_ns > a.system_cpu_ns * 7);
    }

    #[test]
    fn refuses_a_real_device() {
        let g = IoRunGlobals {
            io_device: "/dev/vda".into(),
            mem_buffer_size: 4096,
        };
        assert!(matches!(
            resolve(2_097_152, &g),
            Err(IoRunRefusal::UnsupportedDevice { .. })
        ));
    }

    #[test]
    fn refuses_above_rtapp_int_saturation() {
        let g = IoRunGlobals {
            io_device: "/dev/null".into(),
            mem_buffer_size: 65_536,
        };
        assert!(matches!(
            resolve(RTAPP_IORUN_MAX_DECLARED_BYTES + 1, &g),
            Err(IoRunRefusal::DeclaredBytesSaturate { .. })
        ));
        assert!(resolve(RTAPP_IORUN_MAX_DECLARED_BYTES, &g).is_ok());
    }

    #[test]
    fn refuses_outside_the_buffer_and_call_domains() {
        let small = IoRunGlobals {
            io_device: "/dev/null".into(),
            mem_buffer_size: 512,
        };
        assert!(matches!(
            resolve(2_097_152, &small),
            Err(IoRunRefusal::MemBufferSizeOutsideDomain { .. })
        ));
        let g = IoRunGlobals {
            io_device: "/dev/null".into(),
            mem_buffer_size: 4096,
        };
        // rt-app's own default buffer makes a nominally large iorun tiny.
        assert!(matches!(
            resolve(4096, &g),
            Err(IoRunRefusal::CallsOutsideDomain { calls: 1 })
        ));
        assert!(matches!(
            resolve(2_147_483_647, &g),
            Err(IoRunRefusal::CallsOutsideDomain { .. })
        ));
    }

    /// rt-app's defaults are what a config gets when it says nothing, and they
    /// put a plausible-looking declaration far outside the domain. The profile
    /// must refuse it rather than answer.
    ///
    /// Note *which* refusal: the 4 MiB default buffer is itself outside the
    /// calibrated buffer range, so that check fires before the call-count one.
    /// The substantive point is the line below it — `"iorun": 8388608` under
    /// rt-app's own defaults is **two** `write` calls, so anything that read it
    /// as "8 MiB of I/O" would be wrong by about five orders of magnitude.
    #[test]
    fn rtapp_defaults_put_a_plausible_declaration_outside_the_domain() {
        let g = IoRunGlobals::default();
        assert_eq!(g.mem_buffer_size, 4 * 1024 * 1024);
        assert_eq!(write_calls(8 * 1024 * 1024, g.mem_buffer_size), 2);
        assert!(matches!(
            resolve(8 * 1024 * 1024, &g),
            Err(IoRunRefusal::MemBufferSizeOutsideDomain {
                mem_buffer_size: 4_194_304
            })
        ));
        // And with a buffer inside the domain, the same declaration is still
        // refused, now on the call count it actually resolves to.
        let in_domain = IoRunGlobals {
            io_device: "/dev/null".into(),
            mem_buffer_size: 1_048_576,
        };
        assert!(matches!(
            resolve(8 * 1024 * 1024, &in_domain),
            Err(IoRunRefusal::CallsOutsideDomain { calls: 8 })
        ));
    }

    #[test]
    fn rounding_is_half_up_and_does_not_overflow() {
        assert_eq!(system_cpu_ns(0), 0);
        // 218625/2048 is not an integer; check the tie convention explicitly.
        assert_eq!(system_cpu_ns(1), 107);
        assert_eq!(system_cpu_ns(2048), 218_625);
        assert_eq!(system_cpu_ns(u64::MAX), u64::MAX);
    }
}
