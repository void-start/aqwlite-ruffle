//! Runtime ABC bytecode patches applied to the officially-served AQW game
//! client, kept separate from the `ruffle_core` engine fork.
//!
//! This crate only touches a matched SWF's `DoAbc`/`DoAbc2` tag, using
//! `swf`'s own ABC reader/writer. It is wired in through the generic hook
//! registered by `ruffle_core::tag_utils::set_swf_patch_hook`; the engine
//! itself has no knowledge of AQW-specific content.

mod patches;
mod tag_splice;

const GAME_SWF_NAME: &str = "Game3098r24.swf";

/// Entry point matching `ruffle_common::tag_utils::set_swf_patch_hook`'s
/// expected signature.
pub fn patch(url: &str, version: u8, data: &mut Vec<u8>) {
    if !is_game_swf(url) {
        return;
    }

    let Some((header_start, body_range, tag_code)) = tag_splice::find_do_abc_tag(data, version)
    else {
        tracing::warn!(target: "aqw_patch", "no DoAbc/DoAbc2 tag found in {url}, skipping patches");
        return;
    };

    let body = data[body_range.clone()].to_vec();
    let (prefix, abc_bytes): (&[u8], &[u8]) = if tag_code == swf::TagCode::DoAbc2 as u16 {
        match tag_splice::split_do_abc2_body(&body) {
            Ok((prefix, abc)) => (prefix, abc),
            Err(e) => {
                tracing::warn!(target: "aqw_patch", "failed to parse DoAbc2 header: {e}");
                return;
            }
        }
    } else {
        (&[], &body)
    };

    let mut abc = match swf::avm2::read::Reader::new(abc_bytes).read() {
        Ok(abc) => abc,
        Err(e) => {
            tracing::warn!(target: "aqw_patch", "failed to parse ABC in {url}: {e}");
            return;
        }
    };

    let mut applied = 0u32;
    match patches::player_domain_cache::apply(&mut abc) {
        Ok(true) => applied += 1,
        Ok(false) => {
            tracing::warn!(target: "aqw_patch", "PlayerDomainCache class not found, skipping")
        }
        Err(e) => tracing::warn!(target: "aqw_patch", "PlayerDomainCache patch skipped: {e}"),
    }
    match patches::handle_socket_data::apply(&mut abc) {
        Ok(true) => applied += 1,
        Ok(false) => {
            tracing::warn!(target: "aqw_patch", "SmartFoxClient class not found, skipping")
        }
        Err(e) => tracing::warn!(target: "aqw_patch", "handleSocketData patch skipped: {e}"),
    }

    if applied == 0 {
        return;
    }

    let mut new_abc_bytes = Vec::new();
    if let Err(e) = swf::avm2::write::Writer::new(&mut new_abc_bytes).write(abc) {
        tracing::warn!(target: "aqw_patch", "failed to re-encode patched ABC: {e}");
        return;
    }

    let mut new_body = prefix.to_vec();
    new_body.extend_from_slice(&new_abc_bytes);

    tag_splice::splice_tag(data, header_start, body_range, tag_code, &new_body);
    // WARN, not INFO: this target isn't covered by the default `RUST_LOG`
    // filter's `ruffle=info` clause, and this fires at most once per matched
    // SWF per session, so it's cheap to always surface.
    tracing::warn!(target: "aqw_patch", "applied {applied} patch(es) to {url}");
}

fn is_game_swf(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next() == Some(GAME_SWF_NAME)
}
