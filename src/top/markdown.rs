//! Markdown as a task's view shows it: its marks taken out and what they mean shown in styles,
//! much as Codex shows its messages. It reads a line at a time, and knows only what agents
//! write: headings, lists, quotes, rules, fenced code, tables, and in a line bold, italic, code,
//! links and escapes. Anything else stays as written.

use std::ops::Range;

use super::{Seg, breaks, columns};

/// Styles of the text in a line.
#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    link: bool,
}

const CODE: &str = "36";
const LINK: &str = "36;4";
/// What frames the text: list markers, quote bars, rules, the target of a link.
const FRAME: &str = "2";

impl Style {
    fn sgr(self) -> &'static str {
        match self {
            Style {
                code: true,
                bold: true,
                ..
            } => "1;36",
            Style { code: true, .. } => CODE,
            Style { link: true, .. } => LINK,
            Style {
                bold: true,
                italic: true,
                ..
            } => "1;3",
            Style { bold: true, .. } => "1",
            Style { italic: true, .. } => "3",
            _ => "",
        }
    }
}

/// A line of markdown to show: what goes before its first row and before the rows it wraps
/// to, and its text. Code wraps too: a view that does not scroll would hide what is cut.
struct Line {
    lead: Vec<Seg>,
    hang: Vec<Seg>,
    body: Vec<Seg>,
}

impl Line {
    fn plain(body: Vec<Seg>) -> Line {
        Line {
            lead: Vec::new(),
            hang: Vec::new(),
            body,
        }
    }

    fn is_blank(&self) -> bool {
        self.body.is_empty() && self.lead.is_empty()
    }

    /// Its rows, broken to `room` columns, or one row without it.
    fn rows(self, room: Option<usize>) -> Vec<Vec<Seg>> {
        let Some(room) = room else {
            let mut row = self.lead;
            row.extend(self.body);
            return vec![row];
        };
        wrap_segs(&self.body, room.saturating_sub(segs_columns(&self.lead)))
            .into_iter()
            .enumerate()
            .map(|(i, piece)| {
                let mut row = match i {
                    0 => self.lead.clone(),
                    _ => self.hang.clone(),
                };
                row.extend(piece);
                row
            })
            .collect()
    }
}

/// Markdown `lines` as rows of up to `room` columns, or a row for each without it. Blank lines
/// between blocks are kept, one where there are several. Links to files in `cwd` show their
/// paths from there.
pub(super) fn render(lines: &[String], room: Option<usize>, cwd: &str) -> Vec<Vec<Seg>> {
    let mut out: Vec<Line> = Vec::new();
    // Lines of a paragraph, or of a table, until it ends.
    let mut paragraph: Vec<String> = Vec::new();
    let mut table: Vec<String> = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let flush = |paragraph: &mut Vec<String>, table: &mut Vec<String>, out: &mut Vec<Line>| {
        if !paragraph.is_empty() {
            let text = std::mem::take(paragraph).join("\n");
            out.extend(split_lines(spans(&text, Style::default(), cwd)));
        }
        if !table.is_empty() {
            out.extend(lined_up(&std::mem::take(table), room, cwd));
        }
    };
    for line in lines {
        let line = line.replace('\t', "    ");
        let t = line.trim_start();
        let marker = fence_marker(t);
        if let Some((c, n)) = fence {
            match marker {
                Some((m, k)) if m == c && k >= n && t.trim_end().chars().all(|x| x == c) => {
                    fence = None
                }
                _ => out.push(Line::plain(vec![(line.clone(), CODE)])),
            }
            continue;
        }
        if t.starts_with('|') {
            if !paragraph.is_empty() {
                flush(&mut paragraph, &mut Vec::new(), &mut out);
            }
            table.push(t.to_string());
            continue;
        }
        if let Some(m) = marker {
            flush(&mut paragraph, &mut table, &mut out);
            fence = Some(m);
            continue;
        }
        if t.is_empty() {
            flush(&mut paragraph, &mut table, &mut out);
            if out.last().is_some_and(|l| !l.is_blank()) {
                out.push(Line::plain(Vec::new()));
            }
            continue;
        }
        match block(&line, cwd) {
            Some(l) => {
                flush(&mut paragraph, &mut table, &mut out);
                out.push(l);
            }
            None => {
                flush(&mut Vec::new(), &mut table, &mut out);
                paragraph.push(line);
            }
        }
    }
    flush(&mut paragraph, &mut table, &mut out);
    while out.last().is_some_and(Line::is_blank) {
        out.pop();
    }
    out.into_iter().flat_map(|l| l.rows(room)).collect()
}

/// One line of markdown on one row, its marks taken out: for a list of tasks.
pub(super) fn inline(text: &str) -> Vec<Seg> {
    spans(text, Style::default(), "")
}

/// The fence a line opens or closes a code block with: three or more backticks or tildes.
fn fence_marker(t: &str) -> Option<(char, usize)> {
    ['`', '~']
        .into_iter()
        .map(|c| (c, t.chars().take_while(|x| *x == c).count()))
        .find(|(_, n)| *n >= 3)
}

/// A line that is a block of its own: a heading, a rule, a quote or an item of a list.
fn block(line: &str, cwd: &str) -> Option<Line> {
    let t = line.trim_start();
    let indent = " ".repeat(columns(line) - columns(t));
    if let Some(level) = heading(t) {
        // `#`s at its end close it only after a space, unlike the one of `C#`.
        let text = t[level..].trim();
        let text = match text.trim_end_matches('#') {
            closed if closed.is_empty() || closed.ends_with(' ') => closed.trim_end(),
            _ => text,
        };
        let style = Style {
            bold: true,
            ..Style::default()
        };
        return Some(Line::plain(spans(text, style, cwd)));
    }
    let rule = |c: char| t.chars().all(|x| x == c || x == ' ') && t.matches(c).count() >= 3;
    if ['-', '*', '_'].into_iter().any(rule) {
        return Some(Line::plain(vec![("───".into(), FRAME)]));
    }
    if t.starts_with('>') {
        let marks = t.chars().take_while(|c| *c == '>' || *c == ' ');
        let bar = vec![(
            format!(
                "{indent}{}",
                "│ ".repeat(marks.filter(|c| *c == '>').count())
            ),
            FRAME,
        )];
        let style = Style {
            italic: true,
            ..Style::default()
        };
        return Some(Line {
            lead: bar.clone(),
            hang: bar,
            ..Line::plain(spans(t.trim_start_matches(['>', ' ']), style, cwd))
        });
    }
    let (marker, text) = item(t)?;
    let lead = format!("{indent}{marker} ");
    Some(Line {
        hang: vec![(" ".repeat(columns(&lead)), "")],
        lead: vec![(lead, FRAME)],
        ..Line::plain(spans(text, Style::default(), cwd))
    })
}

/// The level of a heading: the `#`s before its text.
fn heading(t: &str) -> Option<usize> {
    let level = t.chars().take_while(|c| *c == '#').count();
    let after = t[level..].chars().next();
    ((1..=6).contains(&level) && after.is_none_or(|c| c == ' ')).then_some(level)
}

/// A list item's marker as shown, and its text: `-`, `*` and `+` as a bullet, numbers kept.
fn item(t: &str) -> Option<(String, &str)> {
    if let Some(text) = ["- ", "* ", "+ "].iter().find_map(|m| t.strip_prefix(m)) {
        return Some(("•".into(), text.trim_start()));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    let rest = &t[digits..];
    let text = rest
        .strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))?;
    (1..=9)
        .contains(&digits)
        .then(|| (t[..digits + 1].to_string(), text.trim_start()))
}

/// A table's rows with its columns lined up two spaces apart, its header bold and ruled under,
/// as Codex shows one; or as written when that is wider than `room`.
fn lined_up(rows: &[String], room: Option<usize>, cwd: &str) -> Vec<Line> {
    let rows: Vec<Vec<String>> = rows.iter().map(|r| cells(r)).collect();
    // Only the row under the header rules it: a row of dashes below is data.
    let header = rows.len() > 1
        && rows[1]
            .iter()
            .all(|c| c.contains('-') && c.chars().all(|x| "-: ".contains(x)));
    let styled: Vec<Vec<Vec<Seg>>> = rows
        .iter()
        .enumerate()
        .filter(|(i, _)| !header || *i != 1)
        .map(|(i, r)| {
            let style = Style {
                bold: header && i == 0,
                ..Style::default()
            };
            r.iter().map(|c| spans(c, style, cwd)).collect()
        })
        .collect();
    let count = styled.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0; count];
    for row in &styled {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(segs_columns(cell));
        }
    }
    let total = widths.iter().sum::<usize>() + 2 * count.saturating_sub(1);
    if room.is_some_and(|room| total > room) {
        return rows
            .iter()
            .map(|r| {
                Line::plain(spans(
                    &format!("| {} |", r.join(" | ")),
                    Style::default(),
                    cwd,
                ))
            })
            .collect();
    }
    let mut out = Vec::new();
    for (i, row) in styled.into_iter().enumerate() {
        let mut body = Vec::new();
        for (j, (mut cell, w)) in row.into_iter().zip(&widths).enumerate() {
            let pad = w - segs_columns(&cell);
            if j + 1 < count {
                cell.push((" ".repeat(pad + 2), ""));
            }
            body.extend(cell);
        }
        out.push(Line::plain(body));
        if header && i == 0 {
            out.push(Line::plain(vec![("━".repeat(total), FRAME)]));
        }
    }
    out
}

/// A table row's cells, split at the bars outside code spans.
fn cells(row: &str) -> Vec<String> {
    let chars: Vec<char> = row.trim().chars().collect();
    let mut out = vec![String::new()];
    let mut i = 0;
    while i < chars.len() {
        let next = match chars[i] {
            '|' => {
                out.push(String::new());
                i + 1
            }
            _ => {
                let next = skip(&chars, i).min(chars.len());
                out.last_mut().unwrap().extend(&chars[i..next]);
                next
            }
        };
        i = next;
    }
    // The bars at its ends close no cell.
    if chars.first() == Some(&'|') {
        out.remove(0);
    }
    if chars.last() == Some(&'|') && chars.len() > 1 {
        out.pop();
    }
    out.into_iter().map(|c| c.trim().to_string()).collect()
}

/// Text styled as its marks tell, with the marks taken out.
fn spans(text: &str, style: Style, cwd: &str) -> Vec<Seg> {
    let chars: Vec<char> = text.chars().collect();
    let mut cells = Vec::new();
    styled(&chars, style, cwd, &mut cells);
    group(&cells)
}

/// Lines from styled text with line breaks in it. A line's indent goes before each row it
/// wraps to.
fn split_lines(segs: Vec<Seg>) -> Vec<Line> {
    let mut lines = vec![Vec::new()];
    for (text, style) in segs {
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Vec::new());
            }
            if !piece.is_empty() {
                lines.last_mut().unwrap().push((piece.to_string(), style));
            }
        }
    }
    lines
        .into_iter()
        .map(|mut body: Vec<Seg>| {
            let indent = match body.first_mut() {
                Some((text, "")) => {
                    let trimmed = text.trim_start_matches(' ');
                    let indent = text.len() - trimmed.len();
                    *text = trimmed.to_string();
                    indent
                }
                _ => 0,
            };
            let lead: Vec<Seg> = match indent {
                0 => Vec::new(),
                n => vec![(" ".repeat(n), "")],
            };
            Line {
                lead: lead.clone(),
                hang: lead,
                ..Line::plain(body)
            }
        })
        .collect()
}

/// Characters in a row with the style of each, taken together where the style is the same.
fn group(cells: &[(char, &'static str)]) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    for &(c, sgr) in cells {
        match out.last_mut() {
            Some((text, s)) if *s == sgr => text.push(c),
            _ => out.push((c.to_string(), sgr)),
        }
    }
    out
}

/// Styled text broken into rows of up to `room` columns.
fn wrap_segs(segs: &[Seg], room: usize) -> Vec<Vec<Seg>> {
    let cells: Vec<(char, &'static str)> = segs
        .iter()
        .flat_map(|(text, sgr)| text.chars().map(move |c| (c, *sgr)))
        .collect();
    let chars: Vec<char> = cells.iter().map(|(c, _)| *c).collect();
    breaks(&chars, room)
        .into_iter()
        .map(|r| group(&cells[r]))
        .collect()
}

fn segs_columns(segs: &[Seg]) -> usize {
    segs.iter().map(|(t, _)| columns(t)).sum()
}

/// Whether a character next to an emphasis mark leaves it outside a word: not a letter, a digit
/// or what joins a path or an expression, so that `src/**/*.rs`, `2**10` and `snake_case`
/// keep their marks.
fn outside(c: Option<char>) -> bool {
    c.is_none_or(|c| !c.is_ascii_alphanumeric() && !"/*\\_".contains(c))
}

/// Whether a character next to an emphasis mark is the text it marks.
fn inside(c: Option<char>, mark: char) -> bool {
    c.is_some_and(|c| !c.is_whitespace() && c != mark && c != '/')
}

/// How many of `c` are at `i`.
fn run(chars: &[char], i: usize, c: char) -> usize {
    chars[i..].iter().take_while(|x| **x == c).count()
}

/// Where the code span opened at `i` closes: at the next run of as many backticks.
fn code_close(chars: &[char], i: usize) -> Option<usize> {
    let n = run(chars, i, '`');
    let mut j = i + n;
    while j < chars.len() {
        match run(chars, j, '`') {
            0 => j += 1,
            m if m == n => return Some(j),
            m => j += m,
        }
    }
    None
}

/// What follows the mark at `i`, past an escaped character or a whole code span.
fn skip(chars: &[char], i: usize) -> usize {
    match chars[i] {
        '\\' => i + 2,
        '`' => match code_close(chars, i) {
            Some(j) => j + run(chars, j, '`'),
            None => i + run(chars, i, '`'),
        },
        _ => i + 1,
    }
}

/// Where the emphasis opened by the run of `k` marks at `i` closes, if it is opened there.
fn emphasis_close(chars: &[char], i: usize, k: usize) -> Option<usize> {
    let mark = chars[i];
    let before = i.checked_sub(1).map(|j| chars[j]);
    if !outside(before) || !inside(chars.get(i + k).copied(), mark) {
        return None;
    }
    let mut j = i + k;
    while j < chars.len() {
        if chars[j] == mark {
            let m = run(chars, j, mark);
            if m == k && inside(Some(chars[j - 1]), mark) && outside(chars.get(j + m).copied()) {
                return Some(j);
            }
            j += m;
            continue;
        }
        j = skip(chars, j);
    }
    None
}

/// Where the closing bracket or parenthesis of the one at `i` is, past code spans.
fn bracket_close(chars: &[char], i: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '\n' => return None,
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(j);
                }
            }
            _ => {}
        }
        j = skip(chars, j);
    }
    None
}

/// A link at `i`: where its text and its target are, and what follows it.
fn link(chars: &[char], i: usize) -> Option<(Range<usize>, Range<usize>, usize)> {
    let label_end = bracket_close(chars, i, '[', ']')?;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let target_end = bracket_close(chars, label_end + 1, '(', ')')?;
    Some((i + 1..label_end, label_end + 2..target_end, target_end + 1))
}

fn on_the_web(target: &str) -> bool {
    target.starts_with("http://") || target.starts_with("https://")
}

/// Where a link goes, to show after its text: a web address whole, and a file's path from
/// `cwd`. Nothing when the text is all of it, as in `[src/a.rs:12](/D:/p/src/a.rs:12)`.
fn target_shown(text: &str, target: &str, cwd: &str) -> Option<String> {
    if on_the_web(target) {
        return (target != text).then(|| target.to_string());
    }
    let mut path = target
        .strip_prefix("file://")
        .unwrap_or(target)
        .replace('\\', "/");
    // `/D:/…`: a Windows path written as a URL's.
    if path.starts_with('/') && path.get(2..3) == Some(":") {
        path.remove(0);
    }
    let cwd = cwd.replace('\\', "/");
    if let Some(inside) = path
        .strip_prefix(&format!("{cwd}/"))
        .filter(|_| !cwd.is_empty())
    {
        path = inside.to_string();
    }
    let text = text.replace('\\', "/");
    (path != text).then_some(path)
}

/// `chars` styled, starting from `style`, into `out`. Links to files in `cwd` show their paths
/// from there.
fn styled(chars: &[char], style: Style, cwd: &str, out: &mut Vec<(char, &'static str)>) {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if chars.get(i + 1).is_some_and(|n| n.is_ascii_punctuation()) => {
                out.push((chars[i + 1], style.sgr()));
                i += 2;
                continue;
            }
            '`' => {
                let n = run(chars, i, '`');
                if let Some(j) = code_close(chars, i) {
                    let mut code = &chars[i + n..j];
                    // One space on each side lets code start or end with a backtick.
                    if code.len() >= 2 && code[0] == ' ' && code[code.len() - 1] == ' ' {
                        code = &code[1..code.len() - 1];
                    }
                    let sgr = Style {
                        code: true,
                        ..style
                    }
                    .sgr();
                    out.extend(code.iter().map(|c| (*c, sgr)));
                    i = j + n;
                } else {
                    out.extend(chars[i..i + n].iter().map(|c| (*c, style.sgr())));
                    i += n;
                }
                continue;
            }
            '*' | '_' => {
                let k = run(chars, i, c);
                if k <= 3
                    && let Some(j) = emphasis_close(chars, i, k)
                {
                    let inner = Style {
                        bold: style.bold || k >= 2,
                        italic: style.italic || k != 2,
                        ..style
                    };
                    styled(&chars[i + k..j], inner, cwd, out);
                    i = j + k;
                } else {
                    out.extend(chars[i..i + k].iter().map(|c| (*c, style.sgr())));
                    i += k;
                }
                continue;
            }
            '!' | '[' => {
                let at = if c == '!' { i + 1 } else { i };
                if chars.get(at) == Some(&'[')
                    && let Some((label, target, end)) = link(chars, at)
                {
                    let from = out.len();
                    styled(
                        &chars[label],
                        Style {
                            link: true,
                            ..style
                        },
                        cwd,
                        out,
                    );
                    let text: String = out[from..].iter().map(|(c, _)| c).collect();
                    let target: String = chars[target].iter().collect();
                    if let Some(shown) = target_shown(&text, &target, cwd) {
                        out.extend(format!(" ({shown})").chars().map(|c| (c, FRAME)));
                    }
                    i = end;
                    continue;
                }
            }
            '<' => {
                if let Some(len) = chars[i + 1..].iter().position(|c| *c == '>') {
                    let target: String = chars[i + 1..i + 1 + len].iter().collect();
                    if on_the_web(&target) && !target.contains(char::is_whitespace) {
                        let sgr = Style {
                            link: true,
                            ..style
                        }
                        .sgr();
                        out.extend(target.chars().map(|c| (c, sgr)));
                        i += len + 2;
                        continue;
                    }
                }
            }
            _ => {}
        }
        out.push((c, style.sgr()));
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows as text, each piece in brackets with its style when it has one.
    fn shown(rows: &[Vec<Seg>]) -> Vec<String> {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|(t, s)| match *s {
                        "" => t.clone(),
                        s => format!("[{s}|{t}]"),
                    })
                    .collect()
            })
            .collect()
    }

    const CWD: &str = "D:\\vc-proj\\Axiopera";

    fn one(text: &str) -> String {
        shown(&[spans(text, Style::default(), CWD)]).remove(0)
    }

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(String::from).collect()
    }

    #[test]
    fn marks_in_a_line_become_styles() {
        for (text, expected) in [
            ("**Done**: all", "[1|Done]: all"),
            ("里加了**断言**，然后", "里加了[1|断言]，然后"),
            ("an *aside* and _this_", "an [3|aside] and [3|this]"),
            ("***both***", "[1;3|both]"),
            ("**`code`** here", "[1;36|code] here"),
            ("`**x**` stays", "[36|**x**] stays"),
            ("``a `b` c`` and **d**", "[36|a `b` c] and [1|d]"),
            ("`` `tick` ``", "[36|`tick`]"),
            // A path, a glob, an expression or a name keeps its marks.
            (
                "src/**/*.rs and tests/**/*.rs",
                "src/**/*.rs and tests/**/*.rs",
            ),
            ("2**10 and 3**2", "2**10 and 3**2"),
            ("a ** b ** c", "a ** b ** c"),
            ("a * b * c", "a * b * c"),
            ("snake_case_name", "snake_case_name"),
            ("an `open code span", "an `open code span"),
            ("\\*not italic\\*", "*not italic*"),
            // A link shows its text, and where it goes unless the text is all of it: a file's
            // path from the task's directory.
            (
                "see [src/top.rs:1184](/D:/vc-proj/Axiopera/src/top.rs:1184) now",
                "see [36;4|src/top.rs:1184] now",
            ),
            (
                "[top.rs](D:\\vc-proj\\Axiopera\\src\\top.rs)",
                "[36;4|top.rs][2| (src/top.rs)]",
            ),
            (
                "[the parser](/D:/vc-proj/Axiopera/src/parser.rs#L4)",
                "[36;4|the parser][2| (src/parser.rs#L4)]",
            ),
            ("[`top`](src/stop.rs)", "[36|top][2| (src/stop.rs)]"),
            (
                "[the docs](https://example.com/a_(b)) and <https://x.org>",
                "[36;4|the docs][2| (https://example.com/a_(b))] and [36;4|https://x.org]",
            ),
            ("[not a link] (x)", "[not a link] (x)"),
        ] {
            assert_eq!(one(text), expected, "{text}");
        }
    }

    #[test]
    fn blocks_have_their_own_rows() {
        let text = "\n## Findings ##\n## C#\n\n\n1. The reader **drops** a line.\n   - nested\n> quoted\n---\n\
                    ```rust\nlet s = \"```\";\n**not bold**\n```\nText `that spans\ntwo` lines.\n";
        assert_eq!(
            shown(&render(&lines(text), None, CWD)),
            [
                "[1|Findings]",
                "[1|C#]",
                "",
                "[2|1. ]The reader [1|drops] a line.",
                "[2|   • ]nested",
                "[2|│ ][3|quoted]",
                "[2|───]",
                "[36|let s = \"```\";]",
                "[36|**not bold**]",
                "Text [36|that spans]",
                "[36|two] lines.",
            ]
        );
    }

    #[test]
    fn code_blocks_end_only_on_their_own_fence() {
        let text = "~~~\n**x**\n```\n~~~\n````\n```\n**y**\n````\n**z**";
        assert_eq!(
            shown(&render(&lines(text), None, CWD)),
            ["[36|**x**]", "[36|```]", "[36|```]", "[36|**y**]", "[1|z]"]
        );
    }

    #[test]
    fn rows_wrap_under_their_text() {
        let text = "- one **two** three four\n> a quote that wraps\n```\nlet code = 1;\n```";
        assert_eq!(
            shown(&render(&lines(text), Some(12), CWD)),
            [
                "[2|• ]one [1|two]",
                "  three four",
                "[2|│ ][3|a quote]",
                "[2|│ ][3|that wraps]",
                "[36|let code =]",
                "[36|1;]",
            ]
        );
    }

    #[test]
    fn tables_line_up_their_columns_when_they_fit() {
        let text = "| File | What |\n|---|:--|\n| `a|b.rs` | **fixed** |\n| 审查.md | kept \\| too |\n\
                    | - | - |";
        let table = render(&lines(text), Some(40), CWD);
        assert_eq!(
            shown(&table),
            [
                "[1|File]     [1|What]",
                "[2|━━━━━━━━━━━━━━━━━━━]",
                "[36|a|b.rs]   [1|fixed]",
                "审查.md  kept | too",
                "-        -",
            ]
        );
        // Too wide, as written.
        assert_eq!(
            shown(&render(&lines(text), Some(16), CWD))[..2],
            ["| File | What |", "| --- | :-- |"]
        );
    }
}
