use swf::avm2::types::{AbcFile, DefaultValue, Index, Multiname};

const CLASS_NAME: &str = "PlayerDomainCache";
const EXPECTED_OLD_VALUE: i32 = 20;
const NEW_MAX_SIZE: i32 = 8;

/// Lowers `types.PlayerDomainCache`'s default LRU cap for cached per-item
/// `ApplicationDomain`s. The cache is the game's own eviction mechanism
/// (see CLAUDE.md §8, decompiled/AQW-ARCH.md §7.4); this only tightens the
/// existing bound by editing the constructor's default-parameter constant,
/// it adds nothing.
///
/// Returns `Ok(true)` if applied, `Ok(false)` if the class wasn't found
/// (game update moved/renamed it), `Err` if it was found but didn't match
/// the expected shape (safer to skip than to guess).
pub fn apply(abc: &mut AbcFile) -> Result<bool, String> {
    let Some(name_idx) = find_string(&abc.constant_pool.strings, CLASS_NAME) else {
        return Ok(false);
    };

    let Some(multiname_pos) = abc
        .constant_pool
        .multinames
        .iter()
        .position(|m| multiname_name_is(m, name_idx))
    else {
        return Ok(false);
    };
    let multiname_idx = multiname_pos as u32 + 1;

    let Some(instance) = abc
        .instances
        .iter()
        .find(|i| i.name.as_u30() == multiname_idx)
    else {
        return Ok(false);
    };
    let ctor_idx = instance.init_method.as_u30() as usize;

    let Some(ctor) = abc.methods.get_mut(ctor_idx) else {
        return Err("constructor method index out of range".to_string());
    };
    let Some(param) = ctor.params.first_mut() else {
        return Err("PlayerDomainCache constructor has no parameters".to_string());
    };

    let old_value = match param.default_value {
        Some(DefaultValue::Int(idx)) => idx
            .as_u30()
            .checked_sub(1)
            .and_then(|i| abc.constant_pool.ints.get(i as usize))
            .copied(),
        _ => None,
    };
    if old_value != Some(EXPECTED_OLD_VALUE) {
        return Err(format!(
            "unexpected constructor default {old_value:?}, expected {EXPECTED_OLD_VALUE:?}"
        ));
    }

    let new_idx = abc.constant_pool.ints.len() as u32 + 1;
    abc.constant_pool.ints.push(NEW_MAX_SIZE);
    param.default_value = Some(DefaultValue::Int(Index::new(new_idx)));

    Ok(true)
}

fn find_string(strings: &[Vec<u8>], target: &str) -> Option<u32> {
    strings
        .iter()
        .position(|s| s == target.as_bytes())
        .map(|i| i as u32 + 1)
}

fn multiname_name_is(multiname: &Multiname, name_idx: u32) -> bool {
    match multiname {
        Multiname::QName { name, .. } | Multiname::QNameA { name, .. } => {
            name.as_u30() == name_idx
        }
        _ => false,
    }
}
