use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHunk {
    pub old: Range<u32>,
    pub new: Range<u32>,
}

pub fn line_diff(old: &str, new: &str, ignore_whitespace: bool) -> Vec<LineHunk> {
    if ignore_whitespace {
        let old = imara_diff::sources::lines(old)
            .map(|line| {
                line.chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let new = imara_diff::sources::lines(new)
            .map(|line| {
                line.chars()
                    .filter(|ch| !ch.is_whitespace())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let mut input = InternedInput::default();
        input.update_before(old.into_iter());
        input.update_after(new.into_iter());
        compute_line_hunks(&input)
    } else {
        let input = InternedInput::new(
            imara_diff::sources::lines(old),
            imara_diff::sources::lines(new),
        );
        compute_line_hunks(&input)
    }
}

fn compute_line_hunks<T: AsRef<[u8]>>(input: &InternedInput<T>) -> Vec<LineHunk> {
    let mut diff = Diff::compute(Algorithm::Histogram, input);
    diff.postprocess_lines(input);
    diff.hunks()
        .map(|hunk| LineHunk {
            old: hunk.before,
            new: hunk.after,
        })
        .collect()
}
