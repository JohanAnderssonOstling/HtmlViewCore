//! Shared lexical rules and numeric paths for the CFI forms this reader resolves.

#[derive(Default)]
pub(super) struct ParsedPath {
    pub(super) steps: Vec<usize>,
    pub(super) offset: Option<usize>,
}

pub(super) enum Location {
    Point(ParsedPath),
    Range(ParsedPath, ParsedPath),
}

pub(super) struct ParsedCfi<'a> {
    pub(super) package: Vec<usize>,
    pub(super) content: Option<&'a str>,
    pub(super) location: Option<Location>,
}

pub(super) fn needs_escape(character: char) -> bool {
    matches!(character, '^' | '[' | ']' | '(' | ')' | ',' | ';' | '=')
}

/// Visit structural bytes, including assertion openings, but never assertion data.
/// Every consumer rejects unbalanced brackets and invalid or dangling escapes.
fn scan(value: &str, mut visit: impl FnMut(usize, u8) -> Option<()>) -> Option<()> {
    let mut assertion = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if escaped {
            if !needs_escape(byte as char) {
                return None;
            }
            escaped = false;
            continue;
        }
        match byte {
            b'^' if assertion => escaped = true,
            b'^' | b'(' | b')' => return None,
            b'[' if !assertion => {
                assertion = true;
                visit(index, byte)?;
            }
            b'[' => return None,
            b']' if assertion => assertion = false,
            b']' => return None,
            _ if !assertion => visit(index, byte)?,
            _ => {}
        }
    }
    (!assertion && !escaped).then_some(())
}

pub(super) fn components(value: &str, delimiter: u8) -> Option<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0;
    scan(value, |index, byte| {
        if byte == delimiter {
            parts.push(&value[start..index]);
            start = index + 1;
        }
        Some(())
    })?;
    parts.push(&value[start..]);
    Some(parts)
}

pub(super) fn parse_fragment(cfi: &str) -> Option<ParsedCfi<'_>> {
    let inner = cfi.strip_prefix("epubcfi(")?.strip_suffix(')')?;
    let parts = components(inner, b'!')?;
    let (package, content) = match parts.as_slice() {
        [package] => (*package, None),
        [package, content] => (*package, Some(*content)),
        _ => return None,
    };
    Some(ParsedCfi {
        package: package_steps(package)?,
        content,
        location: match content {
            Some(content) => Some(parse_location(content)?),
            None => None,
        },
    })
}

/// Numeric components may carry one optional assertion; assertion text is advisory.
fn integer(value: &str) -> Option<usize> {
    let length = value.bytes().take_while(u8::is_ascii_digit).count();
    let digits = &value[..length];
    if digits.is_empty() || (digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    let mut asserted = false;
    scan(&value[length..], |_, byte| {
        if byte != b'[' || asserted {
            return None;
        }
        asserted = true;
        Some(())
    })?;
    digits.parse().ok()
}

fn parse_path(value: &str) -> Option<ParsedPath> {
    let parts = components(value, b':')?;
    let (path, offset) = match parts.as_slice() {
        [path] => (*path, None),
        [path, offset] => (*path, Some(integer(offset)?)),
        _ => return None,
    };
    if path == "/" && offset.is_some() {
        return None;
    }
    let steps = if path.is_empty() || path == "/" {
        Vec::new()
    } else {
        components(path.strip_prefix('/')?, b'/')?
            .into_iter()
            .map(|step| integer(step).filter(|step| *step > 0))
            .collect::<Option<Vec<_>>>()?
    };
    Some(ParsedPath { steps, offset })
}

fn document_path(path: ParsedPath) -> Option<ParsedPath> {
    if path.steps.is_empty() && path.offset.is_some() {
        return None;
    }
    Some(path)
}

fn parse_location(value: &str) -> Option<Location> {
    let parts = components(value, b',')?;
    match parts.as_slice() {
        [point] => Some(Location::Point(document_path(parse_path(point)?)?)),
        [common, start, end] => {
            let common = parse_path(common)?;
            if common.offset.is_some() {
                return None;
            }
            let extend = |relative| {
                let mut path = parse_path(relative)?;
                drop(path.steps.splice(0..0, common.steps.iter().copied()));
                document_path(path)
            };
            Some(Location::Range(extend(start)?, extend(end)?))
        }
        _ => None,
    }
}

pub(super) fn package_steps(package: &str) -> Option<Vec<usize>> {
    let path = parse_path(package)?;
    if path.offset.is_some()
        || path.steps.is_empty()
        || path.steps.iter().any(|step| !step.is_multiple_of(2))
    {
        return None;
    }
    Some(path.steps)
}

pub(super) fn synthetic_index(package: &[usize]) -> Option<usize> {
    let [6, step] = package else {
        return None;
    };
    (*step >= 2 && step.is_multiple_of(2)).then(|| step / 2 - 1)
}
