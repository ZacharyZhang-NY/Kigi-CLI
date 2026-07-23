//! Test-only helpers for `kigi-hooks`. Gated on `#[cfg(test)]`, so integration
//! tests under `tests/` cannot reach it — they must re-implement what they need.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// Run `f` with the env var `name` set to `value` (or unset if `value`
/// is `None`), restoring the previous value on return.
///
/// The save -> set -> run -> restore lifecycle is panic-safe but not race-safe:
/// `cargo test` runs tests in parallel and env vars are process-global, so
/// callers must pick uniquely-named vars.
///
/// FIXME: nothing enforces the unique-name discipline; a caller passing `HOME`
/// would produce flaky tests. The fix is a `serial_test` dev-dep plus
/// `#[serial(env_var)]` on every env-touching test.
pub(crate) fn with_env_var<R>(name: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
    let previous = std::env::var_os(name);
    // SAFETY: env-var writes are not thread-safe. Callers use uniquely
    // named vars so no concurrent test races on the same name.
    unsafe {
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    let result = catch_unwind(AssertUnwindSafe(f));

    // SAFETY: see above. Restore unconditionally so a panic doesn't
    // leak env state to subsequent tests.
    unsafe {
        match previous {
            Some(prev) => std::env::set_var(name, prev),
            None => std::env::remove_var(name),
        }
    }

    match result {
        Ok(value) => value,
        Err(payload) => resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_previous_value_on_normal_return() {
        let key = "KIGI_HOOKS_TEST_SUPPORT_RESTORE";
        with_env_var(key, Some("first"), || {
            with_env_var(key, Some("second"), || {
                assert_eq!(std::env::var(key).unwrap(), "second");
            });
            assert_eq!(std::env::var(key).unwrap(), "first");
        });
        assert!(std::env::var(key).is_err());
    }

    #[test]
    fn restores_previous_unset_state_on_normal_return() {
        let key = "KIGI_HOOKS_TEST_SUPPORT_UNSET_RESTORE";
        // SAFETY: see the thread-safety note on `with_env_var`.
        unsafe {
            std::env::remove_var(key);
        }
        with_env_var(key, Some("temporary"), || {
            assert_eq!(std::env::var(key).unwrap(), "temporary");
        });
        assert!(std::env::var(key).is_err());
    }

    #[test]
    fn restores_after_panic() {
        let key = "KIGI_HOOKS_TEST_SUPPORT_PANIC_RESTORE";
        // SAFETY: see the thread-safety note on `with_env_var`.
        unsafe {
            std::env::remove_var(key);
        }
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            with_env_var(key, Some("during-panic"), || {
                panic!("intentional");
            });
        }));
        assert!(panicked.is_err(), "expected panic to propagate");
        assert!(
            std::env::var(key).is_err(),
            "env var must be restored after panic"
        );
    }

    #[test]
    fn allows_explicit_unset() {
        let key = "KIGI_HOOKS_TEST_SUPPORT_EXPLICIT_UNSET";
        // SAFETY: see the thread-safety note on `with_env_var`.
        unsafe {
            std::env::set_var(key, "before");
        }
        with_env_var(key, None, || {
            assert!(std::env::var(key).is_err());
        });
        assert_eq!(std::env::var(key).unwrap(), "before");
        // SAFETY: see the thread-safety note on `with_env_var`.
        unsafe {
            std::env::remove_var(key);
        }
    }
}
