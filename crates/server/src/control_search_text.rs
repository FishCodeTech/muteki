pub(crate) fn snippet(text: &str, lower_query: &str, max_chars: usize) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let (start, end) = lowercase_range(&normalized, lower_query)?;
    let chars: Vec<char> = normalized.chars().collect();
    if chars.len() <= max_chars {
        return Some(normalized);
    }

    let hit_start = normalized[..start].chars().count();
    let hit_len = normalized[start..end].chars().count();
    let context = max_chars.saturating_sub(hit_len);
    let window_start = hit_start.saturating_sub(context / 2);
    let window_end = (window_start + max_chars).min(chars.len());
    let window_start = window_end.saturating_sub(max_chars);
    let mut result = String::new();
    if window_start > 0 {
        result.push('…');
    }
    result.extend(chars[window_start..window_end].iter());
    if window_end < chars.len() {
        result.push('…');
    }
    Some(result)
}

fn lowercase_range(text: &str, needle: &str) -> Option<(usize, usize)> {
    if text.is_ascii() {
        let start = text.to_ascii_lowercase().find(needle)?;
        return Some((start, start + needle.len()));
    }

    let folded = text.to_lowercase();
    if text.contains('Σ')
        && let Some((offset, sigma)) = leading_sigma(needle)
    {
        if sigma == 'ς' {
            return find_range(text, &folded, needle, Some((offset, false)));
        }
        let mut alternate = needle.to_owned();
        alternate.replace_range(offset..offset + sigma.len_utf8(), "ς");
        return find_range(text, &folded, needle, None)
            .into_iter()
            .chain(find_range(text, &folded, &alternate, Some((offset, true))))
            .min();
    }
    find_range(text, &folded, needle, None)
}

fn leading_sigma(needle: &str) -> Option<(usize, char)> {
    let (offset, sigma) = needle
        .char_indices()
        .find(|(_, ch)| matches!(ch, 'σ' | 'ς'))?;
    let prefix = &needle[..offset];
    let prefix_preserves_sigma_context = format!("A{prefix}Σ").to_lowercase().ends_with('ς')
        && format!(".{prefix}Σ").to_lowercase().ends_with('σ');
    prefix_preserves_sigma_context.then_some((offset, sigma))
}

fn char_spans(text: &str) -> impl Iterator<Item = (usize, usize, usize, usize)> + '_ {
    text.char_indices().scan(0, |offset, (start, ch)| {
        let lower_start = *offset;
        *offset += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
        Some((lower_start, *offset, start, start + ch.len_utf8()))
    })
}

fn find_range(
    text: &str,
    folded: &str,
    needle: &str,
    sigma: Option<(usize, bool)>,
) -> Option<(usize, usize)> {
    let first = folded.find(needle)?;
    let mut spans = char_spans(text).peekable();
    let mut sigma_spans = char_spans(text);
    let mut starts_on_boundary = |candidate| {
        while spans.peek().is_some_and(|span| span.0 < candidate) {
            spans.next();
        }
        spans.peek().is_some_and(|span| span.0 == candidate)
            && sigma.is_none_or(|(offset, expected_uppercase)| {
                let position = candidate + offset;
                sigma_spans
                    .find(|span| span.1 > position)
                    .is_some_and(|span| text[span.2..span.3].starts_with('Σ') == expected_uppercase)
            })
    };
    let start = if starts_on_boundary(first) {
        first
    } else {
        first
            + 1
            + find_overlapping(
                &folded.as_bytes()[first + 1..],
                needle.as_bytes(),
                |start| starts_on_boundary(first + 1 + start),
            )?
    };
    let source_start = spans.peek()?.2;
    let end = start + needle.len();
    let source_end = spans.find(|span| span.1 >= end)?.3;
    Some((source_start, source_end))
}

fn find_overlapping(
    text: &[u8],
    needle: &[u8],
    mut accept: impl FnMut(usize) -> bool,
) -> Option<usize> {
    let mut prefix = vec![0; needle.len()];
    let mut matched = 0;
    for index in 1..needle.len() {
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        matched += usize::from(needle[index] == needle[matched]);
        prefix[index] = matched;
    }
    matched = 0;
    for (index, byte) in text.iter().enumerate() {
        while matched > 0 && *byte != needle[matched] {
            matched = prefix[matched - 1];
        }
        matched += usize::from(*byte == needle[matched]);
        if matched == needle.len() {
            let start = index + 1 - matched;
            if accept(start) {
                return Some(start);
            }
            matched = prefix[matched - 1];
        }
    }
    None
}
