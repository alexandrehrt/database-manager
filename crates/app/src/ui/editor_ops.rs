//! Editing commands of the SQL editor that egui's text field doesn't have:
//! duplicate / move lines, auto-indent and paired brackets and quotes.
//!
//! Each works on the text and the selection as byte offsets (start <= end)
//! and returns the new text and selection, or None to leave the key to the
//! text field.

/// New text and selection.
pub type Edit = (String, usize, usize);

/// Byte range of the whole lines the selection touches, without the final newline.
fn line_span(text: &str, start: usize, end: usize) -> (usize, usize) {
    let first = text[..start].rfind('\n').map_or(0, |p| p + 1);
    // A selection ending at a line start doesn't include that line.
    let end = if end > start && text[..end].ends_with('\n') { end - 1 } else { end };
    let last = text[end..].find('\n').map_or(text.len(), |p| end + p);
    (first, last)
}

/// Cmd+D: copies the selected lines (or the cursor's line) below themselves
/// and moves the selection onto the copy.
pub fn duplicate_lines(text: &str, start: usize, end: usize) -> Edit {
    let (a, b) = line_span(text, start, end);
    let block = &text[a..b];
    let mut out = String::with_capacity(text.len() + block.len() + 1);
    out.push_str(&text[..b]);
    out.push('\n');
    out.push_str(block);
    out.push_str(&text[b..]);
    let shift = block.len() + 1;
    (out, start + shift, end + shift)
}

/// Alt+Up / Alt+Down: swaps the selected lines with the line above / below.
pub fn move_lines(text: &str, start: usize, end: usize, up: bool) -> Option<Edit> {
    let (a, b) = line_span(text, start, end);
    let block = &text[a..b];
    if up {
        if a == 0 {
            return None;
        }
        let prev = text[..a - 1].rfind('\n').map_or(0, |p| p + 1);
        let above = &text[prev..a - 1];
        let out = format!("{}{block}\n{above}{}", &text[..prev], &text[b..]);
        let shift = a - prev;
        Some((out, start - shift, end - shift))
    } else {
        if b == text.len() {
            return None;
        }
        let next = text[b + 1..].find('\n').map_or(text.len(), |p| b + 1 + p);
        let below = &text[b + 1..next];
        let out = format!("{}{below}\n{block}{}", &text[..a], &text[next..]);
        let shift = below.len() + 1;
        Some((out, start + shift, end + shift))
    }
}

/// Enter: a new line indented like the current one, one level deeper after "(".
pub fn newline(text: &str, start: usize, end: usize) -> Edit {
    let line_start = text[..start].rfind('\n').map_or(0, |p| p + 1);
    let line = &text[line_start..start];
    let mut indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    if line.trim_end().ends_with('(') {
        indent.push_str("    ");
    }
    let insert = format!("\n{indent}");
    let out = format!("{}{insert}{}", &text[..start], &text[end..]);
    let at = start + insert.len();
    (out, at, at)
}

fn closer(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '\'' => Some('\''),
        '"' => Some('"'),
        _ => None,
    }
}

/// Typing `ch`: wraps a selection in brackets / quotes, inserts the closing
/// half of a pair, or steps over a closing character that is already there.
pub fn type_char(text: &str, start: usize, end: usize, ch: char) -> Option<Edit> {
    let next = text[end..].chars().next();
    let prev = text[..start].chars().next_back();
    let quote = ch == '\'' || ch == '"';
    // Step over the closing half that auto-pairing put there.
    if start == end && next == Some(ch) && (quote || matches!(ch, ')' | ']' | '}')) {
        let at = start + ch.len_utf8();
        return Some((text.to_string(), at, at));
    }
    let close = closer(ch)?;
    if start < end {
        let out = format!("{}{ch}{}{close}{}", &text[..start], &text[start..end], &text[end..]);
        return Some((out, start + 1, end + 1));
    }
    let free_after = next.is_none_or(|c| c.is_whitespace() || matches!(c, ')' | ']' | '}' | ',' | ';'));
    // A quote right after a word is an apostrophe or closes a string.
    let free_before = !quote || prev.is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == ch));
    if !(free_after && free_before) {
        return None;
    }
    let out = format!("{}{ch}{close}{}", &text[..start], &text[end..]);
    Some((out, start + 1, start + 1))
}

/// Backspace between an empty pair such as "()" deletes both halves.
pub fn backspace(text: &str, start: usize, end: usize) -> Option<Edit> {
    if start != end {
        return None;
    }
    let prev = text[..start].chars().next_back()?;
    let next = text[start..].chars().next()?;
    if closer(prev) != Some(next) {
        return None;
    }
    let a = start - prev.len_utf8();
    let out = format!("{}{}", &text[..a], &text[start + next.len_utf8()..]);
    Some((out, a, a))
}

/// Cmd+Shift+K: deletes the selected lines (or the cursor's line) and puts the
/// cursor at the start of the line that takes their place.
pub fn delete_lines(text: &str, start: usize, end: usize) -> Edit {
    let (a, b) = line_span(text, start, end);
    // Take the line's newline with it; on the last line, the one before it.
    let (from, to) = if b < text.len() { (a, b + 1) } else { (a.saturating_sub(1), b) };
    let out = format!("{}{}", &text[..from], &text[to..]);
    let at = if b < text.len() { a } else { out[..from].rfind('\n').map_or(0, |p| p + 1) };
    (out, at, at)
}
