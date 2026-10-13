//! Three-way line merge of a note: the server's answer to a write whose
//! base revision is no longer current (`core`). Given the text both sides
//! started from and each side's version, it takes every change either side
//! made, or refuses.
//!
//! The rule that matters: **a merge never drops or reorders a line either
//! side wrote.** So it refuses whenever the two sides' changes touch, not
//! only when they overlap: an edit that ends on the line where the other
//! side's edit starts, or two insertions at the same place, has no order
//! that is obviously right, and a conflict copy is better than a guess.
//! The one exception is the same change made on both sides, taken once.
//!
//! Lines keep their endings (`split_inclusive`), so a merge of texts that
//! agree on line endings and on the final newline reproduces them byte for
//! byte.

use similar::{capture_diff_slices, Algorithm, DiffOp};

/// One side's change against the base: base lines `start..end` replaced by
/// `lines` (an insertion when `start == end`, a deletion when `lines` is
/// empty).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk<'a> {
    start: usize,
    end: usize,
    lines: Vec<&'a str>,
}

fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The side's changes against the base, in base order, with touching ones
/// joined (an equal run separates any two that remain).
fn hunks<'a>(base: &[&'a str], side: &[&'a str]) -> Vec<Hunk<'a>> {
    let mut out: Vec<Hunk<'a>> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, base, side) {
        let (start, end, new) = match op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete { old_index, old_len, new_index } => (old_index, old_index + old_len, new_index..new_index),
            DiffOp::Insert { old_index, new_index, new_len } => (old_index, old_index, new_index..new_index + new_len),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                (old_index, old_index + old_len, new_index..new_index + new_len)
            }
        };
        match out.last_mut() {
            Some(last) if last.end == start => {
                last.end = end;
                last.lines.extend_from_slice(&side[new]);
            }
            _ => out.push(Hunk { start, end, lines: side[new].to_vec() }),
        }
    }
    out
}

/// Merge `ours` and `theirs`, both edited from `base`. `None` when the
/// changes touch (see the module doc) or any text is not UTF-8.
pub fn merge3(base: &[u8], ours: &[u8], theirs: &[u8]) -> Option<Vec<u8>> {
    if ours == theirs || base == theirs {
        return Some(ours.to_vec());
    }
    if base == ours {
        return Some(theirs.to_vec());
    }
    let (base, ours, theirs) =
        (std::str::from_utf8(base).ok()?, std::str::from_utf8(ours).ok()?, std::str::from_utf8(theirs).ok()?);
    merge_text(base, ours, theirs).map(String::into_bytes)
}

/// [`merge3`] on text.
pub fn merge_text(base: &str, ours: &str, theirs: &str) -> Option<String> {
    let base_lines = lines(base);
    let ours_lines = lines(ours);
    let theirs_lines = lines(theirs);
    let mut all: Vec<(bool, Hunk)> = hunks(&base_lines, &ours_lines)
        .into_iter()
        .map(|h| (true, h))
        .chain(hunks(&base_lines, &theirs_lines).into_iter().map(|h| (false, h)))
        .collect();
    all.sort_by_key(|(_, h)| (h.start, h.end));

    // Within one side the hunks are disjoint and separated, so the last
    // hunk taken from each side is the only one the next can touch.
    let mut last: [Option<Hunk>; 2] = [None, None];
    let mut take: Vec<Hunk> = Vec::new();
    for (is_ours, hunk) in all {
        let other = &last[usize::from(is_ours)];
        if let Some(other) = other {
            if hunk.start <= other.end {
                if *other == hunk {
                    continue; // the same change on both sides: once
                }
                return None;
            }
        }
        last[usize::from(!is_ours)] = Some(hunk.clone());
        take.push(hunk);
    }

    let mut out = String::with_capacity(base.len().max(ours.len()).max(theirs.len()));
    let mut at = 0;
    for hunk in &take {
        out.extend(base_lines[at..hunk.start].iter().copied());
        out.extend(hunk.lines.iter().copied());
        at = hunk.end;
    }
    out.extend(base_lines[at..].iter().copied());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(base: &str, ours: &str, theirs: &str) -> Option<String> {
        merge_text(base, ours, theirs)
    }

    #[test]
    fn separate_edits_both_land() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "A\nb\nc\nd\ne\n";
        let theirs = "a\nb\nc\nd\nE\n";
        assert_eq!(m(base, ours, theirs).unwrap(), "A\nb\nc\nd\nE\n");
    }

    #[test]
    fn appends_on_both_sides_conflict() {
        // Two additions at the end of a daily note: no order is right.
        let base = "# Today\n";
        assert_eq!(m(base, "# Today\n- ours\n", "# Today\n- theirs\n"), None);
    }

    #[test]
    fn touching_edits_conflict() {
        let base = "a\nb\nc\n";
        assert_eq!(m(base, "a\nB\nc\n", "a\nb\nC\n"), None, "edits on adjacent lines touch");
        assert_eq!(m(base, "a\nB\nc\n", "a\nB2\nc\n"), None, "the same line edited differently");
    }

    #[test]
    fn identical_change_is_taken_once() {
        let base = "a\nb\nc\nd\ne\n";
        let ours = "a\nB\nc\nd\nE\n";
        let theirs = "a\nB\nc\nd\ne\n";
        assert_eq!(m(base, ours, theirs).unwrap(), "a\nB\nc\nd\nE\n");
    }

    #[test]
    fn deletion_and_distant_insertion() {
        let base = "1\n2\n3\n4\n5\n6\n";
        let ours = "1\n3\n4\n5\n6\n";
        let theirs = "1\n2\n3\n4\n5\nnew\n6\n";
        assert_eq!(m(base, ours, theirs).unwrap(), "1\n3\n4\n5\nnew\n6\n");
    }

    #[test]
    fn missing_final_newline_is_kept() {
        let base = "a\nb\nc\nd";
        assert_eq!(m(base, "A\nb\nc\nd", "a\nb\nc\nD").unwrap(), "A\nb\nc\nD");
    }

    #[test]
    fn trivial_cases() {
        assert_eq!(merge3(b"x", b"y", b"x").unwrap(), b"y");
        assert_eq!(merge3(b"x", b"x", b"z").unwrap(), b"z");
        assert_eq!(merge3(b"x", b"q", b"q").unwrap(), b"q");
        assert_eq!(merge3(b"x\n", &[0xff, b'\n'], b"z\n"), None, "not UTF-8");
    }

    /// A tiny deterministic generator, so the property test needs no crate.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn edit(rng: &mut Rng, base: &[String], tag: &str) -> Vec<String> {
        let mut out = base.to_vec();
        for k in 0..1 + rng.below(3) {
            let at = rng.below(out.len() + 1);
            match rng.below(3) {
                0 => out.insert(at, format!("{tag}{k}-{}\n", rng.next() % 1000)),
                1 if at < out.len() => {
                    out.remove(at);
                }
                _ if at < out.len() => out[at] = format!("{tag}{k}*{}\n", rng.next() % 1000),
                _ => out.push(format!("{tag}{k}+\n")),
            }
        }
        out
    }

    fn is_subsequence(needle: &[&str], hay: &[&str]) -> bool {
        let mut it = hay.iter();
        needle.iter().all(|n| it.any(|h| h == n))
    }

    #[test]
    fn a_merge_never_drops_a_line_either_side_wrote() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let mut merged = 0;
        for _ in 0..3000 {
            let n = 3 + rng.below(12);
            let base: Vec<String> = (0..n).map(|i| format!("line {i}\n")).collect();
            let ours = edit(&mut rng, &base, "o");
            let theirs = edit(&mut rng, &base, "t");
            let (b, o, t) = (base.concat(), ours.concat(), theirs.concat());
            let Some(out) = m(&b, &o, &t) else { continue };
            merged += 1;
            let out_lines = lines(&out);
            for (side, text) in [("ours", &o), ("theirs", &t)] {
                let added: Vec<&str> = hunks(&lines(&b), &lines(text)).into_iter().flat_map(|h| h.lines).collect();
                assert!(
                    is_subsequence(&added, &out_lines),
                    "{side}'s lines lost or reordered\nbase:\n{b}\nours:\n{o}\ntheirs:\n{t}\nmerged:\n{out}"
                );
            }
            assert_eq!(m(&b, &t, &o).as_deref(), Some(out.as_str()), "merge is symmetric");
        }
        assert!(merged > 300, "the generator should produce mergeable pairs (got {merged})");
    }
}
