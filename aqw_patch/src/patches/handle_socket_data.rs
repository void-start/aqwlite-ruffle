use swf::avm2::types::{AbcFile, Exception, Index, MethodBody, Multiname, Op, Trait, TraitKind};
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
    let snippet_exc = snippet_body
        .exceptions
        .first()
        .ok_or("snippet's handleSocketData has no exception handler")?;

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

    let class_namespace_set = class_namespace_set(abc)?;
    let mut table = OverrideTable::build(abc, class_namespace_set)?;

    // Disassemble, remap operand indices, then resolve every branch target
    // and the exception range to *op indices* (not raw byte offsets) before
    // anything is re-encoded. Byte offsets aren't stable across this: some
    // remapped multiname indices (e.g. 22 -> 8930) need more bytes to encode
    // as a u30 than the snippet's own small indices did, which shifts every
    // absolute/relative offset after that point if left untranslated.
    let (positions, mut ops) = disassemble(&snippet_body.code)?;
    let old_total_len = snippet_body.code.len();
    let branch_targets = resolve_branch_targets(&ops, &positions, old_total_len)?;

    let from_idx = find_op_index(&positions, old_total_len, snippet_exc.from_offset as usize);
    let to_idx = find_op_index(&positions, old_total_len, snippet_exc.to_offset as usize);
    let target_idx = find_op_index(&positions, old_total_len, snippet_exc.target_offset as usize);
    if from_idx == usize::MAX || to_idx == usize::MAX || target_idx == usize::MAX {
        return Err("exception range doesn't land on instruction boundaries".to_string());
    }

    for op in &mut ops {
        table.remap_op(op)?;
    }

    // Pass 1: encode every op once (branch offsets zeroed - fixed S24
    // encoding means the offset's value never affects instruction size) to
    // learn each instruction's real new byte position.
    let mut new_positions = Vec::with_capacity(ops.len());
    let mut cursor = 0usize;
    for op in &ops {
        new_positions.push(cursor);
        let mut sized = op.clone();
        if branch_offset(&sized).is_some() {
            set_branch_offset(&mut sized, 0);
        }
        cursor += encode_one(&sized)?.len();
    }
    let new_total_len = cursor;

    // Pass 2: now that new positions are known, fix up every branch offset
    // and the exception range to match.
    for (i, target) in branch_targets.iter().enumerate() {
        if let Some(target) = target {
            let after = op_end_position(&new_positions, new_total_len, i);
            let target_pos = op_position(&new_positions, new_total_len, *target);
            set_branch_offset(&mut ops[i], target_pos as i32 - after as i32);
        }
    }

    let new_code = reassemble(&ops)?;

    // The method body's own traits declare the activation object's local
    // variable slots (`chunk`, `avail`, ... and their types). These aren't
    // touched by disassembling `code` at all, but they reference the same
    // snippet-local multiname indices and need the same translation - this
    // was missed on the first pass and silently produced a body whose
    // locals were typed against whatever unrelated multiname happened to
    // sit at that index in the target's (much larger) pool, surfacing as a
    // `TypeError: Type Coercion failed` the first time real socket data
    // exercised the method.
    let new_traits =
        table.remap_traits(abc, class_namespace_set, &snippet, &snippet_body.traits)?;

    let new_body = MethodBody {
        method: Index::new(target_method_idx as u32),
        max_stack: snippet_body.max_stack,
        num_locals: snippet_body.num_locals,
        init_scope_depth: snippet_body.init_scope_depth,
        max_scope_depth: snippet_body.max_scope_depth,
        code: new_code,
        exceptions: vec![Exception {
            from_offset: op_position(&new_positions, new_total_len, from_idx) as u32,
            to_offset: op_position(&new_positions, new_total_len, to_idx) as u32,
            target_offset: op_position(&new_positions, new_total_len, target_idx) as u32,
            variable_name: exc_var,
            type_name: exc_type,
        }],
        traits: new_traits,
    };
    abc.method_bodies[target_body_idx] = new_body;

    Ok(true)
}

/// The byte position immediately after op `i` - where a branch instruction's
/// offset is measured from. `positions.len()` (i.e. `total_len`) if `i` is
/// the last op.
fn op_end_position(positions: &[usize], total_len: usize, i: usize) -> usize {
    positions.get(i + 1).copied().unwrap_or(total_len)
}

/// The byte position of op index `i`, or `total_len` for the sentinel index
/// `positions.len()` (used when an offset points exactly at the end of the
/// method body, e.g. a try-block that runs to the last instruction).
fn op_position(positions: &[usize], total_len: usize, i: usize) -> usize {
    positions.get(i).copied().unwrap_or(total_len)
}

/// Resolves an absolute byte offset to an op index. Returns `positions.len()`
/// (a valid sentinel, see `op_position`/`op_end_position`) if `target` is
/// exactly the end of the method body, or `usize::MAX` if it doesn't land on
/// any instruction boundary at all.
fn find_op_index(positions: &[usize], total_len: usize, target: usize) -> usize {
    if let Some(i) = positions.iter().position(|&p| p == target) {
        return i;
    }
    if target == total_len {
        return positions.len();
    }
    usize::MAX
}

fn resolve_branch_targets(
    ops: &[Op],
    positions: &[usize],
    total_len: usize,
) -> Result<Vec<Option<usize>>, String> {
    ops.iter()
        .enumerate()
        .map(|(i, op)| {
            let Some(offset) = branch_offset(op) else {
                return Ok(None);
            };
            let after = op_end_position(positions, total_len, i);
            let target_pos = after as i64 + offset as i64;
            if target_pos < 0 {
                return Err(format!("op {i} branches to a negative position"));
            }
            let target_pos = target_pos as usize;
            let idx = find_op_index(positions, total_len, target_pos);
            if idx == usize::MAX {
                return Err(format!(
                    "op {i} branches to byte {target_pos}, which isn't an instruction boundary"
                ));
            }
            Ok(Some(idx))
        })
        .collect()
}

fn branch_offset(op: &Op) -> Option<i32> {
    match *op {
        Op::IfEq { offset }
        | Op::IfFalse { offset }
        | Op::IfGe { offset }
        | Op::IfGt { offset }
        | Op::IfLe { offset }
        | Op::IfLt { offset }
        | Op::IfNe { offset }
        | Op::IfNge { offset }
        | Op::IfNgt { offset }
        | Op::IfNle { offset }
        | Op::IfNlt { offset }
        | Op::IfStrictEq { offset }
        | Op::IfStrictNe { offset }
        | Op::IfTrue { offset }
        | Op::Jump { offset } => Some(offset),
        _ => None,
    }
}

fn set_branch_offset(op: &mut Op, new_offset: i32) {
    match op {
        Op::IfEq { offset }
        | Op::IfFalse { offset }
        | Op::IfGe { offset }
        | Op::IfGt { offset }
        | Op::IfLe { offset }
        | Op::IfLt { offset }
        | Op::IfNe { offset }
        | Op::IfNge { offset }
        | Op::IfNgt { offset }
        | Op::IfNle { offset }
        | Op::IfNlt { offset }
        | Op::IfStrictEq { offset }
        | Op::IfStrictNe { offset }
        | Op::IfTrue { offset }
        | Op::Jump { offset } => *offset = new_offset,
        _ => unreachable!("set_branch_offset called on a non-branch op"),
    }
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
/// references - in its bytecode *and* in its method-body traits (the
/// activation object's local-variable slot declarations) - to an equivalent
/// entry in the target ABC, either an existing one (found by name) or a
/// freshly appended one. Built by resolving each symbol by name rather than
/// hardcoding snippet-side constant-pool index numbers, so a recompile of
/// the snippet that renumbers its own pool doesn't silently break this.
struct OverrideTable {
    /// snippet multiname index -> target multiname index
    multinames: std::collections::HashMap<u32, u32>,
    /// snippet string index -> target string index (only `PushString`)
    strings: std::collections::HashMap<u32, u32>,
}

impl OverrideTable {
    fn build(target: &mut AbcFile, class_namespace_set: u32) -> Result<Self, String> {
        let snippet = swf::avm2::read::Reader::new(SNIPPET_ABC)
            .read()
            .map_err(|e| format!("failed to parse embedded snippet ABC: {e}"))?;

        let mut multinames = std::collections::HashMap::new();
        let mut strings = std::collections::HashMap::new();

        for symbol in [
            "ByteArray",
            "Event",
            "String",
            "int",
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

    /// Translates the method body's own traits (the activation object's
    /// local-variable slot declarations). Slot *names* are arbitrary labels
    /// specific to this snippet (`chunk`, `avail`, ...) with no equivalent
    /// in the target, so those are appended fresh on demand rather than
    /// looked up in the curated symbol list above; slot *types* (`ByteArray`,
    /// `int`, ...) go through the same curated entries the bytecode uses.
    fn remap_traits(
        &mut self,
        target: &mut AbcFile,
        class_namespace_set: u32,
        snippet: &AbcFile,
        traits: &[Trait],
    ) -> Result<Vec<Trait>, String> {
        let qname_namespace = top_level_namespace(target)?;
        traits
            .iter()
            .map(|t| {
                let name = self.resolve_trait_name(target, qname_namespace, snippet, t.name.as_u30())?;
                let kind = match t.kind {
                    TraitKind::Slot {
                        slot_id,
                        type_name,
                        value,
                    } => TraitKind::Slot {
                        slot_id,
                        type_name: Index::new(self.resolve_or_append(
                            target,
                            class_namespace_set,
                            snippet,
                            type_name.as_u30(),
                        )?),
                        value,
                    },
                    TraitKind::Const {
                        slot_id,
                        type_name,
                        value,
                    } => TraitKind::Const {
                        slot_id,
                        type_name: Index::new(self.resolve_or_append(
                            target,
                            class_namespace_set,
                            snippet,
                            type_name.as_u30(),
                        )?),
                        value,
                    },
                    ref other => {
                        return Err(format!("unexpected trait kind in method body: {other:?}"));
                    }
                };
                Ok(Trait {
                    name: Index::new(name),
                    kind,
                    metadata: t.metadata.clone(),
                    is_final: t.is_final,
                    is_override: t.is_override,
                })
            })
            .collect()
    }

    fn resolve_or_append(
        &mut self,
        target: &mut AbcFile,
        class_namespace_set: u32,
        snippet: &AbcFile,
        snippet_idx: u32,
    ) -> Result<u32, String> {
        if snippet_idx == 0 {
            return Ok(0); // the "any type" wildcard, valid as-is
        }
        if let Some(&idx) = self.multinames.get(&snippet_idx) {
            return Ok(idx);
        }
        let m = snippet
            .constant_pool
            .multinames
            .get(snippet_idx as usize - 1)
            .ok_or_else(|| format!("snippet multiname {snippet_idx} out of range"))?;
        let name_idx = multiname_name_idx(m)
            .ok_or_else(|| format!("snippet multiname {snippet_idx} has no name to resolve"))?;
        let symbol = snippet
            .constant_pool
            .strings
            .get(name_idx as usize - 1)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .ok_or_else(|| format!("snippet multiname {snippet_idx} has a dangling string"))?;
        let target_idx = find_or_append_target_multiname(target, &symbol, class_namespace_set)?;
        self.multinames.insert(snippet_idx, target_idx);
        Ok(target_idx)
    }

    /// Trait *names* (as opposed to their types) are fixed, compile-time
    /// bindings - the verifier expects a `QName`, not the open
    /// `Multiname`-with-namespace-set kind `find_or_append_target_multiname`
    /// produces for property/method access. These labels (`chunk`, `avail`,
    /// ...) are also snippet-local with no equivalent in the target, so this
    /// always appends fresh rather than trying to match an existing one.
    fn resolve_trait_name(
        &mut self,
        target: &mut AbcFile,
        qname_namespace: u32,
        snippet: &AbcFile,
        snippet_idx: u32,
    ) -> Result<u32, String> {
        if snippet_idx == 0 {
            return Ok(0);
        }
        if let Some(&idx) = self.multinames.get(&snippet_idx) {
            return Ok(idx);
        }
        let m = snippet
            .constant_pool
            .multinames
            .get(snippet_idx as usize - 1)
            .ok_or_else(|| format!("snippet multiname {snippet_idx} out of range"))?;
        let name_idx = multiname_name_idx(m)
            .ok_or_else(|| format!("snippet multiname {snippet_idx} has no name to resolve"))?;
        let symbol = snippet
            .constant_pool
            .strings
            .get(name_idx as usize - 1)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .ok_or_else(|| format!("snippet multiname {snippet_idx} has a dangling string"))?;
        let target_idx = find_or_append_target_qname(target, &symbol, qname_namespace)?;
        self.multinames.insert(snippet_idx, target_idx);
        Ok(target_idx)
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

/// Same as `find_or_append_target_multiname`, but always produces (or
/// reuses) a `QName` under a single fixed namespace instead of an open
/// `Multiname`-with-namespace-set. Used for trait names, which the AVM2
/// verifier expects to be fixed bindings.
fn find_or_append_target_qname(
    target: &mut AbcFile,
    symbol: &str,
    namespace: u32,
) -> Result<u32, String> {
    let str_idx = if let Some(idx) = find_string(&target.constant_pool.strings, symbol) {
        idx
    } else {
        target.constant_pool.strings.push(symbol.as_bytes().to_vec());
        target.constant_pool.strings.len() as u32
    };
    if let Some(idx) = target.constant_pool.multinames.iter().position(|m| {
        matches!(m, Multiname::QName { namespace: ns, name }
            if ns.as_u30() == namespace && name.as_u30() == str_idx)
    }) {
        return Ok(idx as u32 + 1);
    }
    target.constant_pool.multinames.push(Multiname::QName {
        namespace: Index::new(namespace),
        name: Index::new(str_idx),
    });
    Ok(target.constant_pool.multinames.len() as u32)
}

/// The namespace ordinary top-level QNames (like `int`'s own) live under in
/// the target pool - reused here for freshly-appended trait-name QNames so
/// they're indistinguishable from ones a real compile would have produced.
fn top_level_namespace(target: &AbcFile) -> Result<u32, String> {
    let str_idx = find_string(&target.constant_pool.strings, "int")
        .ok_or("target ABC has no \"int\" string")?;
    target
        .constant_pool
        .multinames
        .iter()
        .find_map(|m| match m {
            Multiname::QName { namespace, name } if name.as_u30() == str_idx => {
                Some(namespace.as_u30())
            }
            _ => None,
        })
        .ok_or("target ABC has no top-level QName for \"int\" to derive a namespace from".to_string())
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

fn disassemble(code: &[u8]) -> Result<(Vec<usize>, Vec<Op>), String> {
    let mut reader = swf::avm2::read::Reader::new(code);
    let mut positions = Vec::new();
    let mut ops = Vec::new();
    loop {
        if reader.as_slice().is_empty() {
            break;
        }
        let pos = code.len() - reader.as_slice().len();
        let op = reader
            .read_op()
            .map_err(|e| format!("bad opcode at byte {pos}: {e}"))?;
        positions.push(pos);
        ops.push(op);
    }
    Ok((positions, ops))
}

fn encode_one(op: &Op) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    swf::avm2::write::Writer::new(&mut out)
        .write_op(op)
        .map_err(|e| format!("failed to write op {op:?}: {e}"))?;
    Ok(out)
}

fn reassemble(ops: &[Op]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    for op in ops {
        out.extend_from_slice(&encode_one(op)?);
    }
    Ok(out)
}
