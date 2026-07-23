//! Macros for measuring and logging execution time.

/// Measure and optionally log execution time of a block.
///
/// # Variants
///
/// - `timed!({ ... })` - Returns `(value, elapsed_ms)` tuple
/// - `timed!(log: "name", { ... })` - Logs at debug level, returns value
/// - `timed!(log: info, "name", { ... })` - Logs at specified level, returns value
/// - `timed!(try: "name", { ... })` - For sync Result blocks, logs and returns Result
/// - `timed!(try: info, "name", { ... })` - For sync Result blocks with level
/// - `timed!(try: "name", async { ... })` - For async Result blocks
/// - `timed!(try: info, "name", async { ... })` - For async Result blocks with level
#[macro_export]
macro_rules! timed {
    (@log_ok $lvl:ident, $name:expr, $elapsed_ms:expr) => {{
        ::tracing::$lvl!(elapsed_ms = $elapsed_ms as u64, "{}", $name);
    }};
    (@log_err $lvl:ident, $name:expr, $elapsed_ms:expr, $err:expr) => {{
        ::tracing::$lvl!(elapsed_ms = $elapsed_ms as u64, error = ?$err, "{}", $name);
    }};

    ($block:block) => {{
        let start = ::std::time::Instant::now();
        let value = $block;
        let elapsed_ms = start.elapsed().as_millis();
        (value, elapsed_ms)
    }};

    (log: $name:expr, $block:block) => {{
        let start = ::std::time::Instant::now();
        let value = $block;
        let elapsed_ms = start.elapsed().as_millis();
        $crate::timed!(@log_ok debug, $name, elapsed_ms);
        value
    }};

    (log: $lvl:ident, $name:expr, $block:block) => {{
        let start = ::std::time::Instant::now();
        let value = $block;
        let elapsed_ms = start.elapsed().as_millis();
        $crate::timed!(@log_ok $lvl, $name, elapsed_ms);
        value
    }};

    // The `try:` arms wrap the block in an immediately-invoked closure (or
    // async block) so a `?` inside it returns into the macro, which can then
    // log the error and elapsed time, rather than escaping the caller.
    (try: $name:expr, $block:block) => {{
        let start = ::std::time::Instant::now();
        let result = (|| $block)();
        let elapsed_ms = start.elapsed().as_millis();
        match result {
            Ok(value) => {
                $crate::timed!(@log_ok debug, $name, elapsed_ms);
                Ok(value)
            }
            Err(err) => {
                $crate::timed!(@log_err debug, $name, elapsed_ms, err);
                Err(err)
            }
        }
    }};

    (try: $lvl:ident, $name:expr, $block:block) => {{
        let start = ::std::time::Instant::now();
        let result = (|| $block)();
        let elapsed_ms = start.elapsed().as_millis();
        match result {
            Ok(value) => {
                $crate::timed!(@log_ok $lvl, $name, elapsed_ms);
                Ok(value)
            }
            Err(err) => {
                $crate::timed!(@log_err $lvl, $name, elapsed_ms, err);
                Err(err)
            }
        }
    }};

    (try: $name:expr, async $block:block) => {{
        let start = ::std::time::Instant::now();
        let result = (async $block).await;
        let elapsed_ms = start.elapsed().as_millis();
        match result {
            Ok(value) => {
                $crate::timed!(@log_ok debug, $name, elapsed_ms);
                Ok(value)
            }
            Err(err) => {
                $crate::timed!(@log_err debug, $name, elapsed_ms, err);
                Err(err)
            }
        }
    }};

    (try: $lvl:ident, $name:expr, async $block:block) => {{
        let start = ::std::time::Instant::now();
        let result = (async $block).await;
        let elapsed_ms = start.elapsed().as_millis();
        match result {
            Ok(value) => {
                $crate::timed!(@log_ok $lvl, $name, elapsed_ms);
                Ok(value)
            }
            Err(err) => {
                $crate::timed!(@log_err $lvl, $name, elapsed_ms, err);
                Err(err)
            }
        }
    }};
}
