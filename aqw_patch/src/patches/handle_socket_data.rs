use swf::avm2::types::{AbcFile, Index, MethodBody, Multiname, Op, TraitKind};
use swf::extensions::ReadSwfExt;

/// Pre-compiled replacement for `SmartFoxClient.handleSocketData`, built from
/// `handle_socket_data_source.as` via Ruffle's own `asc.jar` + its
/// `playerglobal_import.abc` (see that file's header comment for the exact
/// command). Checked in pre-compiled so building `aqw_patch` doesn't need
/// Java; regenerate by hand if the source changes.
const SNIPPET_ABC: &[u8] = include_bytes!("handle_socket_data.abc");

const CLASS_NAME: &str = "SmartFoxClient";
const METHOD_NAME: &str = "handleSocketData";
const SNIPPET_CLASS_NAME: &str = "HSD";

/// Replaces `SmartFoxClient.handleSocketData`'s byte-at-a-time socket read
/// loop with a bulk `readBytes` + in-memory scan for the same `0x00`-delimited
/// message framing. Same observable behavior (including the per-message
/// try/catch around `handleMessage`), fewer native calls and one less
/// `ByteArray` allocation per message (`clear()` instead of replacing
/// `byteBuffer`). See CLAUDE.md §8: this is the hot path for every server
/// message during combat.
pub fn apply(abc: &mut AbcFile) -> Result<bool, String> {
    let Some(target_method_idx) = find_method_index(abc, CLASS_NAME, METHOD_NAME) else {
        return Ok(false);
    };

    let snippet = swf::avm2::read::Reader::new(SNIPPET_ABC)
        .read()
        .map_err(|e| format!("failed to parse embedded snippet ABC: {e}"))?;
    let Some(snippet_method_idx) = find_method_index(&snippet, SNIPPET_CLASS_NAME, METHOD_NAME)
    else {
        return Err("snippet ABC is missing handleSocketData".to_string());
    };
    let snippet_body = snippet
        .method_bodies
        .iter()
        .find(|b| b.method.as_u30() as usize == snippet_method_idx)
        .ok_or("snippet ABC is missing handleSocketData's method body")?;

    let target_body_idx = abc
        .method_bodies
        .iter()
        .position(|b| b.method.as_u30() as usize == target_method_idx)
        .ok_or("handleSocketData has no method body")?;
    let (exc_type, exc_var) = {
        let exc = abc.method_bodies[target_body_idx]
            .exceptions
            .first()
            .ok_or("handleSocketData's original body has no exception handler; refusing to replace it without one to steal namespaces from")?;
        (exc.type_name, exc.variable_name)
    };

    let table = OverrideTable::build(abc)?;

    let mut ops = disassemble(&snippet_body.code)
        .map_err(|e| format!("failed to disassemble snippet method body: {e}"))?;
    for op in &mut ops {
        table.remap_op(op)?;
    }
    let new_code =
        reassemble(&ops).map_err(|e| format!("failed to reassemble patched bytecode: {e}"))?;

    let new_body = MethodBody {
        method: Index::new(target_method_idx as u32),
        max_stack: snippet_body.max_stack,
        num_locals: snippet_body.num_locals,
        init_scope_depth: snippet_body.init_scope_depth,
        max_scope_depth: snippet_body.max_scope_depth,
        code: new_code,
        exceptions: vec![swf::avm2::types::Exception {
            from_offset: snippet_body.exceptions[0].from_offset,
            to_offset: snippet_body.exceptions[0].to_offset,
            target_offset: snippet_body.exceptions[0].target_offset,
            variable_name: exc_var,
            type_name: exc_type,
        }],
        traits: snippet_body.traits.clone(),
    };
    abc.method_bodies[target_body_idx] = new_body;

    Ok(true)
}

fn find_method_index(abc: &AbcFile, class_name: &str, method_name: &str) -> Option<usize> {
    let class_name_idx = find_string(&abc.constant_pool.strings, class_name)?;
    let class_multiname_idx = abc
        .constant_pool
        .multinames
        .iter()
        .position(|m| matches!(m, Multiname::QName { name, .. } if name.as_u30() == class_name_idx))?
        as u32
        + 1;
    let instance = abc
        .instances
        .iter()
        .find(|i| i.name.as_u30() == class_multiname_idx)?;

    let method_name_idx = find_string(&abc.constant_pool.strings, method_name)?;
    instance.traits.iter().find_map(|t| {
        let is_match = abc
            .constant_pool
            .multinames
            .get(t.name.as_u30() as usize - 1)
            .is_some_and(|m| matches!(m, Multiname::QName { name, .. } if name.as_u30() == method_name_idx));
        if !is_match {
            return None;
        }
        match t.kind {
            TraitKind::Method { method, .. } => Some(method.as_u30() as usize),
            _ => None,
        }
    })
}

fn find_string(strings: &[Vec<u8>], target: &str) -> Option<u32> {
    strings
        .iter()
        .position(|s| s == target.as_bytes())
        .map(|i| i as u32 + 1)
}

/// Maps every multiname/string the snippet's `handleSocketData` body
/// references to an equivalent entry in the target ABC, either an existing
/// one (found by name) or a freshly appended one. Built once per patch
/// application by resolving each symbol used, rather than hardcoding
/// snippet-side constant-pool index numbers, so a recompile of the snippet
/// that renumbers its own pool doesn't silently break this.
struct OverrideTable {
    /// snippet multiname index -> target multiname index
    multinames: std::collections::HashMap<u32, u32>,
    /// snippet string index -> target string index (only `PushString`)
    strings: std::collections::HashMap<u32, u32>,
}

impl OverrideTable {
    fn build(target: &mut AbcFile) -> Result<Self, String> {
        // (symbol name, snippet multiname index) pairs, read off the
        // snippet's own constant pool by name so this doesn't depend on
        // exact index numbers surviving a recompile.
        let snippet = swf::avm2::read::Reader::new(SNIPPET_ABC)
            .read()
            .map_err(|e| format!("failed to parse embedded snippet ABC: {e}"))?;

        let class_namespace_set = class_namespace_set(target)?;
        let mut multinames = std::collections::HashMap::new();
        let mut strings = std::collections::HashMap::new();

        for symbol in [
            "ByteArray",
            "socketConnection",
            "bytesAvailable",
            "readBytes",
            "length",
            "byteBuffer",
            "writeBytes",
            "toString",
            "clear",
            "handleMessage",
            "message",
            "debugMessage",
            "int",
        ] {
            let Some(snippet_str_idx) = find_string(&snippet.constant_pool.strings, symbol)
            else {
                continue;
            };
            // The snippet's compiler can allocate more than one multiname
            // entry for the same symbol (e.g. one for its own trait
            // declaration, another for a call site) - map every one of
            // them to the same target index, not just the first.
            let snippet_multiname_idxs: Vec<u32> = snippet
                .constant_pool
                .multinames
                .iter()
                .enumerate()
                .filter(|(_, m)| multiname_name_idx(m) == Some(snippet_str_idx))
                .map(|(i, _)| i as u32 + 1)
                .collect();
            if snippet_multiname_idxs.is_empty() {
                continue;
            }
            let target_idx =
                find_or_append_target_multiname(target, symbol, class_namespace_set)?;
            for idx in snippet_multiname_idxs {
                multinames.insert(idx, target_idx);
            }
        }

        // The dynamic `chunk[i]` byte index: a `MultinameL` with no name
        // component. Reuse whichever one the target's own compiled code
        // already uses for this exact pattern instead of guessing at a
        // namespace set.
        if let Some(snippet_idx) = snippet
            .constant_pool
            .multinames
            .iter()
            .position(|m| matches!(m, Multiname::MultinameL { .. }))
            .map(|i| i as u32 + 1)
        {
            let target_idx = target
                .constant_pool
                .multinames
                .iter()
                .position(|m| matches!(m, Multiname::MultinameL { .. }))
                .map(|i| i as u32 + 1)
                .ok_or("target ABC has no MultinameL to reuse for dynamic byte indexing")?;
            multinames.insert(snippet_idx, target_idx);
        }

        // The literal error-log string; already used verbatim by the
        // original `handleSocketData`.
        if let Some(snippet_str_idx) =
            find_string(&snippet.constant_pool.strings, "handleMessage error: ")
        {
            let target_str_idx = find_string(&target.constant_pool.strings, "handleMessage error: ")
                .ok_or("target ABC is missing the \"handleMessage error: \" string")?;
            strings.insert(snippet_str_idx, target_str_idx);
        }

        Ok(Self { multinames, strings })
    }

    fn remap_op(&self, op: &mut Op) -> Result<(), String> {
        let index = match op {
            Op::FindPropStrict { index }
            | Op::ConstructProp { index, .. }
            | Op::Coerce { index }
            | Op::GetProperty { index }
            | Op::CallProperty { index, .. }
            | Op::CallPropVoid { index, .. } => Some(index),
            _ => None,
        };
        if let Some(index) = index {
            let new_idx = *self
                .multinames
                .get(&index.as_u30())
                .ok_or_else(|| format!("no override for multiname index {}", index.as_u30()))?;
            *index = Index::new(new_idx);
            return Ok(());
        }

        if let Op::PushString { value } = op {
            let new_idx = *self
                .strings
                .get(&value.as_u30())
                .ok_or_else(|| format!("no override for string index {}", value.as_u30()))?;
            *value = Index::new(new_idx);
        }
        Ok(())
    }
}

fn multiname_name_idx(m: &Multiname) -> Option<u32> {
    match m {
        Multiname::QName { name, .. } | Multiname::QNameA { name, .. } => Some(name.as_u30()),
        Multiname::Multiname { name, .. } | Multiname::MultinameA { name, .. } => {
            Some(name.as_u30())
        }
        _ => None,
    }
}

/// Finds an existing target multiname referencing `symbol` under the
/// target class's own namespace set, or appends a new one (and a new
/// string, if `symbol` isn't in the pool yet either) that reuses that same
/// namespace set. Appending is always safe here: these are additions to
/// the *end* of each pool array, so no existing index anywhere else in the
/// ABC shifts.
fn find_or_append_target_multiname(
    target: &mut AbcFile,
    symbol: &str,
    class_namespace_set: u32,
) -> Result<u32, String> {
    if let Some(str_idx) = find_string(&target.constant_pool.strings, symbol) {
        // Prefer an existing multiname under the class's own namespace set...
        if let Some(idx) = target.constant_pool.multinames.iter().position(|m| {
            matches!(m, Multiname::Multiname { namespace_set, name }
                if namespace_set.as_u30() == class_namespace_set && name.as_u30() == str_idx)
        }) {
            return Ok(idx as u32 + 1);
        }
        // ...otherwise any existing multiname for this name at all.
        if let Some(idx) = target
            .constant_pool
            .multinames
            .iter()
            .position(|m| multiname_name_idx(m) == Some(str_idx))
        {
            return Ok(idx as u32 + 1);
        }

        // Name known, but never referenced under this namespace set: append
        // a fresh multiname reusing the existing string + namespace set.
        target.constant_pool.multinames.push(Multiname::Multiname {
            namespace_set: Index::new(class_namespace_set),
            name: Index::new(str_idx),
        });
        return Ok(target.constant_pool.multinames.len() as u32);
    }

    // Neither the string nor a multiname for it exist: append both.
    target.constant_pool.strings.push(symbol.as_bytes().to_vec());
    let str_idx = target.constant_pool.strings.len() as u32;
    target.constant_pool.multinames.push(Multiname::Multiname {
        namespace_set: Index::new(class_namespace_set),
        name: Index::new(str_idx),
    });
    Ok(target.constant_pool.multinames.len() as u32)
}

fn class_namespace_set(target: &AbcFile) -> Result<u32, String> {
    // `socketConnection` is always referenced from within `SmartFoxClient`
    // itself, so whatever namespace set it resolves through in the target
    // is exactly the "this class's own scope" set every other symbol here
    // needs too.
    let str_idx = find_string(&target.constant_pool.strings, "socketConnection")
        .ok_or("target ABC has no \"socketConnection\" string")?;
    target
        .constant_pool
        .multinames
        .iter()
        .find_map(|m| match m {
            Multiname::Multiname { namespace_set, name } if name.as_u30() == str_idx => {
                Some(namespace_set.as_u30())
            }
            _ => None,
        })
        .ok_or("target ABC has no \"socketConnection\" multiname to derive the class namespace set from".to_string())
}

fn disassemble(code: &[u8]) -> Result<Vec<Op>, String> {
    let mut reader = swf::avm2::read::Reader::new(code);
    let mut ops = Vec::new();
    loop {
        if reader.as_slice().is_empty() {
            break;
        }
        let op = reader
            .read_op()
            .map_err(|e| format!("bad opcode at byte {}: {e}", code.len() - reader.as_slice().len()))?;
        ops.push(op);
    }
    Ok(ops)
}

fn reassemble(ops: &[Op]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut writer = swf::avm2::write::Writer::new(&mut out);
    for op in ops {
        writer
            .write_op(op)
            .map_err(|e| format!("failed to write op {op:?}: {e}"))?;
    }
    Ok(out)
}
