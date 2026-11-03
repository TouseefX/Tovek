//! The executor-side Luau host scripts, embedded from the repository root so
//! every platform build of the GUI carries them. Each decompile transport has
//! its own client:
//!
//!   * `decompile.client.luau` — one POST per script to `/decompile`
//!     (base64 body), driven by SynSaveInstance's `getgenv().decompile` hook.
//!   * `decompile-batch.client.luau` — the BATCH + RAW transports: whole-game
//!     pre-walk, one request per chunk to `/decompile/batch` (binary `MDB1`
//!     framing, or base64-in-JSON with `USE_RAW = false`), single-POST
//!     fallback per cache miss.

/// Single-script client (POST /decompile, base64 body).
pub const SINGLE_CLIENT: &str = include_str!("../../decompile.client.luau");

/// Batch/RAW client (POST /decompile/batch + /decompile/raw, MDB1 or JSON).
pub const BATCH_CLIENT: &str = include_str!("../../decompile-batch.client.luau");

const DEFAULT_ORIGIN: &str = "http://localhost:3000";

/// The single-script client, retargeted at `host` (e.g. `192.168.1.7:3000`
/// on Android, where executors cannot reach `localhost`, or
/// `localhost:<port>` on desktop).
pub fn single_client_for_host(host: &str) -> String {
    retarget(SINGLE_CLIENT, host)
}

/// The batch/RAW client, retargeted at `host` (see [`single_client_for_host`]).
pub fn batch_client_for_host(host: &str) -> String {
    retarget(BATCH_CLIENT, host)
}

fn retarget(script: &str, host: &str) -> String {
    if host == "localhost:3000" {
        return script.to_owned();
    }
    script.replace(DEFAULT_ORIGIN, &format!("http://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripts_embed_the_default_origin() {
        assert!(SINGLE_CLIENT.contains(DEFAULT_ORIGIN));
        assert!(BATCH_CLIENT.contains(DEFAULT_ORIGIN));
    }

    #[test]
    fn retargeting_rewrites_every_url() {
        let single = single_client_for_host("localhost:4123");
        let batch = batch_client_for_host("192.168.1.7:3000");
        assert!(!single.contains(DEFAULT_ORIGIN));
        assert!(!batch.contains(DEFAULT_ORIGIN));
        assert!(single.contains("http://localhost:4123/decompile"));
        assert!(batch.contains("http://192.168.1.7:3000"));
    }

    #[test]
    fn default_host_is_untouched() {
        assert_eq!(single_client_for_host("localhost:3000"), SINGLE_CLIENT);
    }
}
