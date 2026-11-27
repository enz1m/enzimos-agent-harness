// SPDX-License-Identifier: MIT
use std::{io::Write, sync::OnceLock, time::Instant};

pub(crate) fn phase(session_id: &str, phase: &str, started: Instant) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if *ENABLED
        .get_or_init(|| std::env::var_os("ENZIMOS_AGENT_TIMING").as_deref() == Some("1".as_ref()))
    {
        let _ = writeln!(
            std::io::stderr().lock(),
            "assistant_timing session={session_id} phase={phase} elapsed_ms={}",
            started.elapsed().as_millis()
        );
    }
}
