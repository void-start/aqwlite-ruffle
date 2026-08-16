use std::ops::Range;
use swf::TagCode;
use swf::extensions::ReadSwfExt;

/// Finds the first `DoAbc`/`DoAbc2` tag in a decompressed SWF tag stream.
/// Returns the tag header's start offset, the byte range of its body, and
/// its tag code.
pub fn find_do_abc_tag(data: &[u8], version: u8) -> Option<(usize, Range<usize>, u16)> {
    let mut reader = swf::read::Reader::new(data, version);
    loop {
        let header_start = reader.pos(data);
        let (tag_code, length) = reader.read_tag_code_and_length().ok()?;
        if tag_code == TagCode::DoAbc as u16 || tag_code == TagCode::DoAbc2 as u16 {
            let body_start = reader.pos(data);
            return Some((header_start, body_start..body_start + length, tag_code));
        }
        if tag_code == TagCode::End as u16 {
            return None;
        }
        reader.read_slice(length).ok()?;
    }
}

/// Splits a `DoAbc2` tag body into its `flags + name` prefix and its raw ABC
/// data. `DoAbc` tags have no such prefix; their whole body is ABC data.
pub fn split_do_abc2_body(body: &[u8]) -> Result<(&[u8], &[u8]), String> {
    let mut reader = swf::read::Reader::new(body, 0);
    reader
        .read_u32()
        .map_err(|e| format!("failed to read DoAbc2 flags: {e}"))?;
    reader
        .read_str()
        .map_err(|e| format!("failed to read DoAbc2 name: {e}"))?;
    let prefix_len = body.len() - reader.get_ref().len();
    Ok(body.split_at(prefix_len))
}

/// Replaces the tag at `header_start`/`body_range` with a freshly-encoded
/// header for `new_body`, splicing directly into the tag stream. Handles the
/// short-form/long-form tag header size change on its own.
pub fn splice_tag(
    data: &mut Vec<u8>,
    header_start: usize,
    body_range: Range<usize>,
    tag_code: u16,
    new_body: &[u8],
) {
    let mut new_header = Vec::with_capacity(6);
    if new_body.len() < 0x3F {
        let tag_code_and_length = (tag_code << 6) | new_body.len() as u16;
        new_header.extend_from_slice(&tag_code_and_length.to_le_bytes());
    } else {
        let tag_code_and_length = (tag_code << 6) | 0x3F;
        new_header.extend_from_slice(&tag_code_and_length.to_le_bytes());
        new_header.extend_from_slice(&(new_body.len() as u32).to_le_bytes());
    }
    data.splice(
        header_start..body_range.end,
        new_header.into_iter().chain(new_body.iter().copied()),
    );
}
