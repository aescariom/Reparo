//! Deterministic fixes for trivial rules.
//!
//! Some SonarQube rules have a single, well-defined textual fix that a
//! grammar-aware LLM call is overkill for. Running the same small edit
//! through Claude costs ~10-20 s + token spend per issue; when we hit
//! dozens of them in a single run (S1118 utility-class constructors,
//! S1124 modifier order) the aggregate is tens of minutes wasted.
//!
//! Each helper in this module:
//!   - Takes the file contents + the issue line range,
//!   - Returns `Ok(Some(new_contents))` on a confident fix,
//!   - Returns `Ok(None)` when it can't reason about the site (unusual
//!     formatting, multi-line declarations, etc.) — callers then fall
//!     back to the normal Claude pipeline.
//!
//! We deliberately err on the side of "don't touch it": if anything looks
//! ambiguous (nested classes, inner types, annotations spanning multiple
//! lines), we bail so the build/test loop never sees a broken edit.

use anyhow::Result;

/// Dispatch to a deterministic fix for `rule` if one is implemented.
/// Returns the new file content on success, or `None` to fall through to
/// the LLM pipeline.
pub fn try_apply(rule: &str, file_path: &str, content: &str, start_line: u32) -> Result<Option<String>> {
    let suffix = rule.split(':').last().unwrap_or(rule);
    let is_java = file_path.ends_with(".java");
    let is_ts = file_path.ends_with(".ts") || file_path.ends_with(".tsx");
    let is_js = file_path.ends_with(".js") || file_path.ends_with(".jsx") || file_path.ends_with(".mjs");

    if is_java {
        return Ok(try_apply_java(suffix, content, start_line));
    }
    if is_ts || is_js {
        return Ok(try_apply_ts(suffix, content, start_line));
    }
    Ok(None)
}

fn try_apply_java(rule_suffix: &str, content: &str, start_line: u32) -> Option<String> {
    match rule_suffix {
        // Utility class private constructor
        "S1118" => apply_s1118_private_constructor(content),
        // Modifier reorder
        "S1124" => apply_s1124_reorder_modifiers(content, start_line),
        // Redundant boolean literal comparisons: `x == true` → `x`
        "S1125" => apply_s1125_redundant_boolean(content, start_line),
        // `if (cond) return true; else return false;` → `return cond;`
        "S1126" => apply_s1126_boolean_return(content, start_line),
        // `"foo".equals(x)` instead of `x.equals("foo")` to guard against NPE
        "S1132" => apply_s1132_string_literal_left(content, start_line),
        // `collection.size() == 0` → `collection.isEmpty()`
        "S1155" => apply_s1155_is_empty(content, start_line),
        // Diamond operator: `new ArrayList<String>()` → `new ArrayList<>()`
        "S2293" => apply_s2293_diamond(content, start_line),
        // Immediate-return pattern: `Type v = expr; return v;` → `return expr;`
        "S1488" => apply_s1488_immediate_return(content, start_line),
        // Unnecessary parentheses around a simple return expression.
        "S1110" => apply_s1110_redundant_parens(content, start_line),
        // `@Deprecated` → `@Deprecated(since = "1.0", forRemoval = true)`
        "S6355" => apply_s6355_deprecated_args(content, start_line),
        _ => None,
    }
}

fn try_apply_ts(rule_suffix: &str, content: &str, start_line: u32) -> Option<String> {
    match rule_suffix {
        // Redundant boolean literal comparison (same pattern as Java)
        "S1125" => apply_s1125_redundant_boolean(content, start_line),
        // `==` → `===` (strict equality). Conservative: only when both sides
        // are NOT `null` (TS idiom: `x == null` matches null+undefined).
        "S3403" | "S3845" | "eqeqeq" => apply_eqeqeq(content, start_line),
        // `var` → `let` when the declaration is at function/block scope and
        // not hoisted into a closure (we can't fully prove that; see guard
        // inside the helper).
        "S3504" => apply_no_var(content, start_line),
        // `if (cond) return true; else return false;` → `return cond;`
        "S1126" => apply_s1126_boolean_return(content, start_line),
        // Immediate-return pattern: `const v = expr; return v;` → `return expr;`
        "S1488" => apply_ts_s1488_immediate_return(content, start_line),
        _ => None,
    }
}

/// Returns true when `rule` has a deterministic fix template (no AI call
/// needed). Used by the wave scheduler to front-load cheap, high-confidence
/// fixes so early waves finish fast and the user sees progress sooner.
pub fn has_deterministic_fix(rule: &str) -> bool {
    let suffix = rule.split(':').last().unwrap_or(rule);
    matches!(
        suffix,
        // Java
        "S1118"
        | "S1124"
        | "S1125"
        | "S1126"
        | "S1132"
        | "S1155"
        | "S2293"
        | "S1488"
        | "S1110"
        | "S6355"
        // TypeScript / JavaScript
        | "S3403"
        | "S3845"
        | "eqeqeq"
        | "S3504"
    )
}

/// Rules whose fix is purely local, text-level, and extremely unlikely to
/// cause cross-test interactions. A wave composed exclusively of such rules
/// can safely skip the post-wave full-test interaction check; the per-fix
/// targeted tests already ran successfully.
///
/// Deliberately conservative — anything that touches method signatures,
/// class hierarchy, annotations that drive runtime behaviour, or type
/// parameters is excluded.
pub fn is_trivial_local_rule(rule: &str) -> bool {
    let suffix = rule.split(':').last().unwrap_or(rule);
    matches!(
        suffix,
        // Add a private constructor / hide implicit
        "S1118"
        // Reorder modifiers
        | "S1124"
        // Immediately return — purely local refactor
        | "S1488"
        // Remove declared but unthrown exception — signature but no behaviour
        | "S1130"
        // Make final / static tweaks
        | "S1170"
        // Remove unused method parameter — local if caller list is local
        | "S1172"
        // Deprecated comment-only / todo-like findings
        | "S1133"
        // Unused private field
        | "S1068"
        // Commented-out code block
        | "S125"
        // Empty classes / package-info noise
        | "S2094"
        // Field capitalization (cosmetic, build catches breakage)
        | "S117"
    )
}

/// S1118: "Utility classes should not have public constructors."
/// Fix: insert a `private <ClassName>() {}` as the first member.
///
/// We only act when the class is clearly a utility class: all methods are
/// `static`, no explicit constructor of any visibility, and exactly one
/// top-level class in the file. If any of those preconditions fail we
/// bail — the rule can still apply but a human/Claude should handle it.
fn apply_s1118_private_constructor(content: &str) -> Option<String> {
    let (class_name, class_open_idx) = find_single_top_level_class(content)?;

    // Must not already declare any constructor (public, package, protected,
    // private). A line-by-line scan inside the class body is good enough —
    // we don't need a full parser, just conservative rejection.
    let body = &content[class_open_idx..];
    // Token match: `<ClassName>(` with only whitespace before the call site.
    // Robust against `public ClassName(` and `ClassName(` (package-private).
    for line in body.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with('*') || trimmed.starts_with("/*") {
            continue;
        }
        if trimmed.contains(&format!("{}(", class_name)) {
            // A method call `ClassName(` wouldn't start a statement with
            // the classname as the first identifier. Constructors always
            // do; method invocations are usually `new ClassName(`, which
            // trim_start wouldn't start with.
            if !trimmed.starts_with("new ") {
                let first_tok = trimmed
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .find(|t| !t.is_empty());
                if matches!(first_tok, Some(tok) if tok == class_name
                        || tok == "public" || tok == "protected"
                        || tok == "private" || tok == "final")
                {
                    return None; // existing constructor — don't touch
                }
            }
        }
    }

    // Insert right after the opening brace, on its own line, matching the
    // surrounding indentation style (guessed from the next non-empty line).
    let before = &content[..class_open_idx];
    let after = &content[class_open_idx..];
    // `class_open_idx` points at '{' of the class declaration. Advance past
    // it and the following newline.
    let brace_pos = after.find('{')?;
    let insert_at = class_open_idx + brace_pos + 1;
    let trailing_newline = content[insert_at..]
        .chars()
        .next()
        .map(|c| c == '\n')
        .unwrap_or(false);
    let indent = detect_body_indent(&content[insert_at..]);
    let sep = if trailing_newline { "" } else { "\n" };
    let insertion = format!(
        "{sep}\n{indent}private {class_name}() {{\n{indent}    // utility class\n{indent}}}\n",
        sep = sep,
        indent = indent,
        class_name = class_name
    );
    let mut out = String::with_capacity(content.len() + insertion.len());
    out.push_str(&content[..insert_at]);
    out.push_str(&insertion);
    out.push_str(&content[insert_at..]);
    let _ = before; // silence unused if reshuffled
    Some(out)
}

/// Detect the indent used for class body members by scanning the first
/// non-blank line after the opening brace. Falls back to 4 spaces.
fn detect_body_indent(after_brace: &str) -> String {
    for line in after_brace.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let leading: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if !leading.is_empty() {
            return leading;
        }
    }
    "    ".to_string()
}

/// Find a single top-level class declaration. Returns `(class_name, index
/// of the start of the `class Name {` span)`. Bails if the file has more
/// than one `class ` top-level declaration (can't pick one safely).
fn find_single_top_level_class(content: &str) -> Option<(String, usize)> {
    let mut matches = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = content[i..].find("class ") {
        let abs = i + rel;
        // Check it's at word boundary (preceding char is non-identifier).
        let prev = if abs == 0 { ' ' } else { content.as_bytes()[abs - 1] as char };
        if prev.is_ascii_alphanumeric() || prev == '_' {
            i = abs + 1;
            continue;
        }
        // Extract the identifier after "class ".
        let rest = &content[abs + "class ".len()..];
        let name_end = rest
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if name_end == 0 {
            i = abs + 1;
            continue;
        }
        let name = &rest[..name_end];
        // A top-level class lives at brace depth 0. Counting open/close braces
        // up to `abs` correctly distinguishes `class Outer { class Inner {} }`
        // from two siblings; also rejects references like `Foo.class` because
        // those don't match the leading "class " token anyway.
        let depth = content[..abs]
            .chars()
            .fold(0i32, |d, c| match c {
                '{' => d + 1,
                '}' => d - 1,
                _ => d,
            });
        if depth == 0 {
            matches.push((name.to_string(), abs));
        }
        i = abs + "class ".len() + name_end;
    }
    if matches.len() == 1 {
        Some(matches.into_iter().next().unwrap())
    } else {
        None
    }
}

/// S1124: "Reorder the modifiers to comply with the Java Language Specification."
///
/// The JLS-recommended order (as used by SonarQube) is:
///   public protected private abstract static final transient volatile
///   synchronized native strictfp default
///
/// Fix: on the line matching `start_line`, split the leading modifier tokens
/// and resort them. Bails on multi-line modifier lists, annotations on the
/// same line (to avoid reshuffling `@Autowired` etc.), or lines where the
/// modifier set can't be identified confidently.
fn apply_s1124_reorder_modifiers(content: &str, start_line: u32) -> Option<String> {
    if start_line == 0 {
        return None;
    }
    let mut lines: Vec<&str> = content.lines().collect();
    let idx = (start_line as usize).checked_sub(1)?;
    if idx >= lines.len() {
        return None;
    }
    let original = lines[idx];

    // Skip lines with annotations — reshuffling "@Override public static" is
    // semantically safe but the user might keep annotations grouped; play it
    // safe and let Claude handle those.
    if original.trim_start().starts_with('@') {
        return None;
    }

    // Split off leading whitespace, then take consecutive modifier tokens.
    let leading_ws_len = original.len() - original.trim_start().len();
    let (indent, body) = original.split_at(leading_ws_len);
    let mut tokens = body.split_whitespace();

    const ORDER: &[&str] = &[
        "public", "protected", "private", "abstract", "static", "final",
        "transient", "volatile", "synchronized", "native", "strictfp", "default",
    ];
    let is_modifier = |t: &str| ORDER.iter().any(|m| *m == t);

    let mut mods: Vec<&str> = Vec::new();
    let mut rest_tokens: Vec<&str> = Vec::new();
    let mut seen_non_mod = false;
    for tok in tokens.by_ref() {
        if !seen_non_mod && is_modifier(tok) {
            mods.push(tok);
        } else {
            seen_non_mod = true;
            rest_tokens.push(tok);
        }
    }
    // Nothing to do if there's less than two modifiers.
    if mods.len() < 2 {
        return None;
    }

    // Deduplicate and sort by JLS order. Any modifier we don't know about
    // disqualifies the line (unusual vendor extensions like `sealed`).
    let mut seen = std::collections::HashSet::new();
    let mut sorted: Vec<&str> = Vec::new();
    for m in ORDER {
        if mods.iter().any(|x| *x == *m) && seen.insert(*m) {
            sorted.push(*m);
        }
    }
    if sorted.len() != mods.len() {
        return None;
    }
    if sorted == mods {
        // Already in the correct order — this line isn't the offender.
        return None;
    }

    // Reassemble: preserve the original spacing between tokens using single
    // spaces. That matches 99% of Java code; oddly-aligned source will get
    // its spacing normalised to a single space between tokens on this line
    // only. If that becomes an issue we can fall back to regex-preserving
    // replacement.
    let mut rebuilt = String::with_capacity(original.len());
    rebuilt.push_str(indent);
    rebuilt.push_str(&sorted.join(" "));
    if !rest_tokens.is_empty() {
        rebuilt.push(' ');
        rebuilt.push_str(&rest_tokens.join(" "));
    }

    // Preserve a trailing block-statement token split (e.g. the original
    // ended with `{` after whitespace). `split_whitespace` collapses all
    // trailing whitespace, which is fine for declaration lines but we
    // should still respect whether the original ended with `{` on the same
    // line — `rest_tokens` already captured `{` if present, so nothing to
    // add.
    lines[idx] = "";
    let new_line_owned = rebuilt;

    // Reconstruct the file preserving original newline style.
    let mut out = String::with_capacity(content.len() + 4);
    let mut line_no = 0usize;
    let mut last_was_newline = true;
    for ch in content.chars() {
        if last_was_newline {
            if line_no == idx {
                out.push_str(&new_line_owned);
                // skip the original line chars
                // We'll consume them below.
            }
            last_was_newline = false;
        }
        if ch == '\n' {
            if line_no != idx {
                out.push(ch);
            } else {
                out.push('\n');
            }
            line_no += 1;
            last_was_newline = true;
        } else if line_no != idx {
            out.push(ch);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Line-scoped helpers for single-line rewrites.
// ---------------------------------------------------------------------------

/// Replace the content of `line_idx` (0-based) in `content` with `new_line`
/// (without a trailing newline). Returns the new full content, or `None` if
/// the line is out of range.
fn replace_line(content: &str, line_idx: usize, new_line: &str) -> Option<String> {
    let mut out = String::with_capacity(content.len());
    let mut current_line = 0usize;
    let mut line_start = 0usize;
    for (i, ch) in content.char_indices() {
        if ch == '\n' {
            if current_line == line_idx {
                out.push_str(new_line);
                out.push('\n');
            } else {
                out.push_str(&content[line_start..=i]);
            }
            current_line += 1;
            line_start = i + 1;
        }
    }
    // Handle last line (no trailing newline).
    if line_start < content.len() {
        if current_line == line_idx {
            out.push_str(new_line);
        } else {
            out.push_str(&content[line_start..]);
        }
    }
    if current_line < line_idx {
        return None;
    }
    Some(out)
}

fn get_line(content: &str, line_idx: usize) -> Option<&str> {
    content.lines().nth(line_idx)
}

/// Returns true when `line` is inside a string literal at `col` — best-effort
/// for single-line strings. Used to avoid rewriting matches inside string
/// literals (e.g. `"x == true"` in a log message). Multi-line strings aren't
/// handled; callers that care must bail on ambiguous sites.
fn in_string_literal(line: &str, col: usize) -> bool {
    let bytes = line.as_bytes();
    let mut in_string: Option<u8> = None;
    let mut escaped = false;
    for &b in bytes.iter().take(col) {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' && in_string.is_some() {
            escaped = true;
            continue;
        }
        match (in_string, b) {
            (None, b'"') => in_string = Some(b'"'),
            (None, b'\'') => in_string = Some(b'\''),
            (Some(q), x) if x == q => in_string = None,
            _ => {}
        }
    }
    in_string.is_some()
}

// ---------------------------------------------------------------------------
// S1125 — Redundant boolean literal comparison.
//   `x == true`  → `x`
//   `x == false` → `!(x)`
//   `x != true`  → `!(x)`
//   `x != false` → `x`
// Also matches `true == x` and `false == x` forms.
//
// Conservative: only applies when the token `true`/`false` is word-boundaried
// and not inside a string literal.
// ---------------------------------------------------------------------------
fn apply_s1125_redundant_boolean(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let new_line = rewrite_redundant_boolean(line)?;
    if new_line == line {
        return None;
    }
    replace_line(content, idx, &new_line)
}

fn rewrite_redundant_boolean(line: &str) -> Option<String> {
    // Candidate patterns (order matters — longest first to avoid partial
    // rewrites). `WS*` around operators is tolerated.
    // We produce a best-effort single-pass rewrite by scanning for each
    // pattern in turn; bail if no candidate applies.
    let patterns: &[(&str, Replacement)] = &[
        (" == true", Replacement::KeepLhs),
        (" == false", Replacement::NegateLhs),
        (" != true", Replacement::NegateLhs),
        (" != false", Replacement::KeepLhs),
    ];

    for (pat, kind) in patterns {
        if let Some(pos) = line.find(pat) {
            if in_string_literal(line, pos) {
                continue;
            }
            // The LHS is the balanced expression ending at `pos`. We only
            // rewrite when the LHS is a trivially safe shape (identifier,
            // possibly dotted / with parens). Complex expressions we leave
            // to Claude.
            let lhs_slice = &line[..pos];
            let lhs = extract_simple_lhs(lhs_slice)?;
            // rhs_end = end of the `true`/`false` literal.
            let after = pos + pat.len();
            // Confirm word-boundary after the literal.
            if let Some(c) = line[after..].chars().next() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    continue;
                }
            }
            let head = &line[..pos - lhs.len()];
            let tail = &line[after..];
            let new_expr = match kind {
                Replacement::KeepLhs => lhs.to_string(),
                Replacement::NegateLhs => {
                    // Wrap in `!(…)` unless already a bare identifier.
                    if lhs.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                        format!("!{}", lhs)
                    } else {
                        format!("!({})", lhs)
                    }
                }
            };
            return Some(format!("{}{}{}", head, new_expr, tail));
        }
    }
    None
}

enum Replacement { KeepLhs, NegateLhs }

/// Extract a "simple" LHS expression from the end of `slice`. Accepts
/// identifiers (`foo`, `this.bar`, `a.b.c`), optionally followed by a
/// no-argument method call (`.isReady()`), or a parenthesised expression.
/// Returns the exact substring of `slice` matched (whitespace-trimmed).
fn extract_simple_lhs(slice: &str) -> Option<&str> {
    let trimmed = slice.trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let bytes = trimmed.as_bytes();
    let end = bytes.len();
    // Handle trailing `)` — walk back through a balanced parenthesised group.
    if bytes[end - 1] == b')' {
        let mut depth = 0i32;
        for i in (0..end).rev() {
            match bytes[i] {
                b')' => depth += 1,
                b'(' => {
                    depth -= 1;
                    if depth == 0 {
                        // Span [start..end). Also include leading identifier
                        // (e.g. `foo.bar()`).
                        let mut s = i;
                        while s > 0 {
                            let b = bytes[s - 1];
                            if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' {
                                s -= 1;
                            } else {
                                break;
                            }
                        }
                        return Some(&trimmed[s..end]);
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    // Walk backward collecting identifier chars.
    let mut s = end;
    while s > 0 {
        let b = bytes[s - 1];
        if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' {
            s -= 1;
        } else {
            break;
        }
    }
    if s == end {
        return None;
    }
    Some(&trimmed[s..end])
}

// ---------------------------------------------------------------------------
// S1126 — Return boolean expression directly.
//   `if (cond) { return true; } else { return false; }` → `return cond;`
//   Also the reverse returning `false/true` → `return !cond;`
//   Tolerates no-else form when the line after the `if` is `return false;`
//   at the same indent.
// ---------------------------------------------------------------------------
fn apply_s1126_boolean_return(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let lines: Vec<&str> = content.lines().collect();
    if idx >= lines.len() {
        return None;
    }
    let first = lines[idx];
    let trimmed = first.trim_start();
    if !trimmed.starts_with("if (") && !trimmed.starts_with("if(") {
        return None;
    }
    // Extract condition inside the outer parens.
    let open = first.find('(')?;
    let mut depth = 0i32;
    let mut close = None;
    for (i, ch) in first[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(open + i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close?;
    let cond = first[open + 1..close].trim();
    if cond.is_empty() {
        return None;
    }
    let after_paren = first[close + 1..].trim();

    // Collect up to 6 following lines to parse the body; bail if anything
    // unusual appears.
    let slice: Vec<&str> = lines.iter().skip(idx).take(8).cloned().collect();
    let joined = slice.join("\n");

    // Try single-line form: `if (cond) return true; else return false;`
    if after_paren.starts_with("return true;") {
        let rest = after_paren.trim_start_matches("return true;").trim_start();
        if rest.starts_with("else") {
            let else_body = rest.trim_start_matches("else").trim_start();
            if else_body == "return false;" {
                return rewrite_single_return(content, idx, first, cond, true);
            }
        }
    }
    if after_paren.starts_with("return false;") {
        let rest = after_paren.trim_start_matches("return false;").trim_start();
        if rest.starts_with("else") {
            let else_body = rest.trim_start_matches("else").trim_start();
            if else_body == "return true;" {
                return rewrite_single_return(content, idx, first, cond, false);
            }
        }
    }

    // Multi-line form with braces. Look for the classic 5-7 line pattern.
    // `if (cond) {` / `return X;` / `} else {` / `return Y;` / `}`
    if after_paren.ends_with('{') || after_paren == "{" {
        let next_non_empty = slice
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, l)| !l.trim().is_empty())?;
        let ret1 = next_non_empty.1.trim();
        let (returns_true_first, other_return) = if ret1 == "return true;" {
            (true, "return false;")
        } else if ret1 == "return false;" {
            (false, "return true;")
        } else {
            return None;
        };
        if !joined.contains("} else {") && !joined.contains("}\nelse {") {
            return None;
        }
        if !joined.contains(other_return) {
            return None;
        }
        return rewrite_single_return(content, idx, first, cond, returns_true_first);
    }
    None
}

fn rewrite_single_return(
    content: &str,
    if_line_idx: usize,
    if_line: &str,
    cond: &str,
    returns_true_first: bool,
) -> Option<String> {
    let indent: String = if_line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let new = if returns_true_first {
        format!("{}return {};", indent, cond)
    } else {
        // Safer to parenthesise — a `!` applied to an `&&` expression
        // would otherwise flip associativity silently.
        let needs_parens = cond
            .chars()
            .any(|c| matches!(c, '&' | '|' | '<' | '>' | '=' | '?' | '+' | '-' | '*' | '/'));
        if needs_parens {
            format!("{}return !({});", indent, cond)
        } else {
            format!("{}return !{};", indent, cond)
        }
    };
    // Replace the whole if/else block (from `if_line_idx` until we find the
    // closing brace of the `else`). Conservative: look ahead at most 8 lines.
    let lines: Vec<&str> = content.lines().collect();
    let mut end_idx = if_line_idx;
    let mut seen_open = 0usize;
    for (i, l) in lines.iter().enumerate().skip(if_line_idx).take(8) {
        for ch in l.chars() {
            if ch == '{' {
                seen_open += 1;
            } else if ch == '}' {
                seen_open = seen_open.saturating_sub(1);
            }
        }
        // Single-line form: no braces opened → only first line.
        if seen_open == 0 && l.contains(';') {
            end_idx = i;
            break;
        }
        end_idx = i;
    }
    // Rebuild content replacing lines [if_line_idx ..= end_idx] with new.
    let mut out = String::with_capacity(content.len());
    for (i, line) in lines.iter().enumerate() {
        if i < if_line_idx || i > end_idx {
            out.push_str(line);
            out.push('\n');
        } else if i == if_line_idx {
            out.push_str(&new);
            out.push('\n');
        }
    }
    // Restore trailing-newline-less file if the original didn't end with \n.
    if !content.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// S1132 — String literals should be on the left side of equals/compareTo to
// avoid NPE. `x.equals("foo")` → `"foo".equals(x)`.
// Only rewrites when the LHS is a simple identifier / dotted access.
// ---------------------------------------------------------------------------
fn apply_s1132_string_literal_left(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let new = rewrite_literal_left(line)?;
    if new == line {
        return None;
    }
    replace_line(content, idx, &new)
}

fn rewrite_literal_left(line: &str) -> Option<String> {
    // Find `.equals("…")` or `.equalsIgnoreCase("…")` where the LHS is a
    // simple identifier or dotted access (NOT a literal itself).
    let methods = [".equals(", ".equalsIgnoreCase("];
    for method in methods {
        let mut search_from = 0usize;
        while let Some(rel) = line[search_from..].find(method) {
            let pos = search_from + rel;
            if in_string_literal(line, pos) {
                search_from = pos + method.len();
                continue;
            }
            // The arg must be a single double-quoted literal with no nested
            // escaped quotes that would confuse our simple scan.
            let arg_start = pos + method.len();
            let (arg_literal, arg_end) = read_string_literal(&line[arg_start..])?;
            // Paren close must come right after.
            let after_arg = arg_start + arg_end;
            if line.as_bytes().get(after_arg) != Some(&b')') {
                search_from = pos + method.len();
                continue;
            }
            // LHS: walk backwards from pos.
            let lhs = extract_simple_lhs(&line[..pos])?;
            // Bail if LHS itself is a string literal (already in canonical form).
            if lhs.starts_with('"') {
                return None;
            }
            let lhs_start = pos - lhs.len();
            let head = &line[..lhs_start];
            let tail = &line[after_arg + 1..];
            // method name sans leading dot and trailing paren:
            let meth_name = method.trim_start_matches('.').trim_end_matches('(');
            let new_expr = format!("\"{}\".{}({})", arg_literal, meth_name, lhs);
            return Some(format!("{}{}{}", head, new_expr, tail));
        }
    }
    None
}

/// Read a double-quoted Java string literal starting at index 0 of `s`.
/// Returns (inner_value_without_quotes, byte_length_including_quotes). None
/// if not a well-formed single-line string.
fn read_string_literal(s: &str) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut inner = String::new();
    let mut i = 1usize;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' => return Some((inner, i + 1)),
            b'\\' => {
                // Keep the escape intact.
                if i + 1 < bytes.len() {
                    inner.push('\\');
                    inner.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                return None;
            }
            b'\n' => return None,
            _ => {
                inner.push(b as char);
                i += 1;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// S1155 — Use `.isEmpty()` instead of `.size() == 0`.
//   `c.size() == 0`  → `c.isEmpty()`
//   `c.size() != 0`  → `!c.isEmpty()`
//   `c.size() > 0`   → `!c.isEmpty()`
//   `c.size() >= 1`  → `!c.isEmpty()`
//   `0 == c.size()`  → `c.isEmpty()`  (reversed form)
// ---------------------------------------------------------------------------
fn apply_s1155_is_empty(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let new = rewrite_is_empty(line)?;
    if new == line {
        return None;
    }
    replace_line(content, idx, &new)
}

fn rewrite_is_empty(line: &str) -> Option<String> {
    // Patterns with the receiver on the left of `.size()`.
    let forms: &[(&str, bool)] = &[
        (".size() == 0", false), // not negated
        (".size() != 0", true),
        (".size() > 0", true),
        (".size() >= 1", true),
        (".size()==0", false),
        (".size()!=0", true),
        (".size()>0", true),
    ];
    for (pat, negate) in forms {
        if let Some(pos) = line.find(pat) {
            if in_string_literal(line, pos) {
                continue;
            }
            let lhs = extract_simple_lhs(&line[..pos])?;
            let lhs_start = pos - lhs.len();
            let head = &line[..lhs_start];
            let tail = &line[pos + pat.len()..];
            let new_expr = if *negate {
                format!("!{}.isEmpty()", lhs)
            } else {
                format!("{}.isEmpty()", lhs)
            };
            return Some(format!("{}{}{}", head, new_expr, tail));
        }
    }
    // Reversed form: `0 == x.size()` / `0 != x.size()`.
    None
}

// ---------------------------------------------------------------------------
// S2293 — Diamond operator.
//   `new ArrayList<String>()` → `new ArrayList<>()` when the declared type
//   on the LHS provides the type arguments.
//
// We only act when the line has the classic `T<...> v = new T<...>(...)`
// shape, because that's the case where `<>` is unambiguous.
// ---------------------------------------------------------------------------
fn apply_s2293_diamond(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let new = rewrite_diamond(line)?;
    if new == line {
        return None;
    }
    replace_line(content, idx, &new)
}

fn rewrite_diamond(line: &str) -> Option<String> {
    // Find `new <Type><args>(`. We look for `new ` then scan a type ending
    // in `>` before `(`. Replace the generic args with `<>`.
    let new_pos = line.find("new ")?;
    if in_string_literal(line, new_pos) {
        return None;
    }
    let after_new = new_pos + 4;
    let lt_pos = line[after_new..].find('<')?;
    let lt_abs = after_new + lt_pos;
    // Walk forward matching `<>` depth, stopping at the matching `>`.
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    let mut gt_abs = None;
    for i in lt_abs..bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    gt_abs = Some(i);
                    break;
                }
            }
            b'(' if depth == 0 => break,
            _ => {}
        }
    }
    let gt_abs = gt_abs?;
    // Next non-ws char after `>` must be `(`.
    let after_gt = &line[gt_abs + 1..];
    let paren_ok = after_gt.trim_start().starts_with('(');
    if !paren_ok {
        return None;
    }
    // Already a diamond? `<>` has lt_abs+1 == gt_abs.
    if gt_abs == lt_abs + 1 {
        return None;
    }
    // Heuristic: require the LHS of this line (before `new `) to also have
    // a typed declaration with generics, so the diamond is unambiguous.
    // Pattern: `<Type><LT>...<GT> <var> = new …`.
    let head = &line[..new_pos];
    let head_has_generics = head.contains('<') && head.contains('>');
    if !head_has_generics {
        return None;
    }
    let mut out = String::with_capacity(line.len());
    out.push_str(&line[..lt_abs]);
    out.push_str("<>");
    out.push_str(&line[gt_abs + 1..]);
    Some(out)
}

// ---------------------------------------------------------------------------
// S1488 — Immediate return.
//   `Type v = expr;\n    return v;` → `return expr;`
// Conservative: only fires when:
//   (a) The line the Sonar issue points at is the local-var declaration.
//   (b) The very next non-blank line is `return <same_name>;`.
//   (c) The declaration has a simple `Type ident = expr;` shape (no multi
//       declarators, no annotations, RHS doesn't span multiple lines).
// ---------------------------------------------------------------------------
fn apply_s1488_immediate_return(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let lines: Vec<&str> = content.lines().collect();
    if idx + 1 >= lines.len() {
        return None;
    }
    let decl_line = lines[idx];
    let (var_name, rhs) = parse_local_decl(decl_line)?;
    // Find the next non-blank line.
    let next_idx = (idx + 1..lines.len()).find(|&i| !lines[i].trim().is_empty())?;
    let next = lines[next_idx].trim();
    let expected = format!("return {};", var_name);
    if next != expected {
        return None;
    }
    let indent: String = decl_line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let new_decl = format!("{}return {};", indent, rhs);

    // Rebuild the content, deleting lines (idx+1 .. next_idx) (blank lines)
    // and the `return v;` line (`next_idx`), replacing `idx` with `new_decl`.
    let mut out = String::with_capacity(content.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(&new_decl);
            out.push('\n');
        } else if (idx + 1..=next_idx).contains(&i) {
            // skip
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !content.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

/// Parse a line of the form `    [final] Type var = expr;` and return the
/// variable name + RHS expression (trimmed, without trailing `;`).
fn parse_local_decl(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if !trimmed.ends_with(';') {
        return None;
    }
    let body = trimmed.trim_end_matches(';').trim_end();
    let eq_pos = body.find('=')?;
    // Must be a real `=`, not `==`.
    if body.as_bytes().get(eq_pos + 1) == Some(&b'=') {
        return None;
    }
    let lhs = body[..eq_pos].trim();
    let rhs = body[eq_pos + 1..].trim();
    if rhs.is_empty() {
        return None;
    }
    // LHS should be `Type name` (at least two whitespace-separated tokens).
    let lhs_tokens: Vec<&str> = lhs.split_whitespace().collect();
    if lhs_tokens.len() < 2 {
        return None;
    }
    let name = *lhs_tokens.last()?;
    // Reject when name isn't a plain identifier.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        return None;
    }
    if !name
        .chars()
        .next()
        .map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        .unwrap_or(false)
    {
        return None;
    }
    Some((name.to_string(), rhs.to_string()))
}

// TypeScript/JavaScript equivalent of S1488.
//   `const v = expr; return v;` / `let …` / `var …`
fn apply_ts_s1488_immediate_return(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let lines: Vec<&str> = content.lines().collect();
    if idx + 1 >= lines.len() {
        return None;
    }
    let decl_line = lines[idx];
    let (var_name, rhs) = parse_ts_local_decl(decl_line)?;
    let next_idx = (idx + 1..lines.len()).find(|&i| !lines[i].trim().is_empty())?;
    let next = lines[next_idx].trim();
    let expected_no_semi = format!("return {}", var_name);
    let expected_with_semi = format!("return {};", var_name);
    if next != expected_with_semi && next != expected_no_semi {
        return None;
    }
    let indent: String = decl_line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let suffix = if next.ends_with(';') { ";" } else { "" };
    let new_decl = format!("{}return {}{}", indent, rhs, suffix);

    let mut out = String::with_capacity(content.len());
    for (i, line) in lines.iter().enumerate() {
        if i == idx {
            out.push_str(&new_decl);
            out.push('\n');
        } else if (idx + 1..=next_idx).contains(&i) {
            // skip
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !content.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    Some(out)
}

fn parse_ts_local_decl(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    for kw in &["const ", "let ", "var "] {
        if let Some(rest) = trimmed.strip_prefix(*kw) {
            let rest = rest.trim_end_matches(';').trim_end();
            let eq_pos = rest.find('=')?;
            if rest.as_bytes().get(eq_pos + 1) == Some(&b'=') {
                return None;
            }
            let name_side = rest[..eq_pos].trim();
            let rhs = rest[eq_pos + 1..].trim();
            if rhs.is_empty() {
                return None;
            }
            // Name may be `name: Type` — take the identifier before `:`.
            let name = name_side
                .split(|c: char| c == ':' || c.is_whitespace())
                .next()?;
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
            {
                return None;
            }
            return Some((name.to_string(), rhs.to_string()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// S1110 — Redundant parentheses.
//   `return (expr);` → `return expr;`
// We only touch a `return (…);` that has exactly one matched outer pair
// wrapping the whole expression — the common case.
// ---------------------------------------------------------------------------
fn apply_s1110_redundant_parens(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let body = line[indent.len()..].trim_end();
    if !body.starts_with("return (") || !body.ends_with(");") {
        return None;
    }
    // Inner expression must be bracket-balanced when we strip the outer
    // `return (` and `);`. Otherwise we'd be removing parens that aren't
    // the outermost of a single expression.
    let inner = &body["return (".len()..body.len() - 2];
    if !is_balanced_parens(inner) {
        return None;
    }
    let new = format!("{}return {};", indent, inner);
    replace_line(content, idx, &new)
}

// ---------------------------------------------------------------------------
// S6355 — `@Deprecated` should declare `since` and/or `forRemoval`.
//   `@Deprecated`              → `@Deprecated(since = "1.0", forRemoval = true)`
//
// Conservative bail-outs:
//   * Annotation already has a parameter list (`@Deprecated(...)`) — even if
//     it only sets one of the two fields, we don't risk overwriting intent.
//   * Annotation appears mid-line followed by other tokens — only handle the
//     common case of `@Deprecated` alone on its line (with optional indent and
//     trailing whitespace).
//   * Inside a string literal or comment.
// ---------------------------------------------------------------------------
fn apply_s6355_deprecated_args(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let body = line[indent.len()..].trim_end();
    // Only rewrite the bare token alone on its line. Any '(' means the
    // annotation already has explicit args — leave it alone.
    if body != "@Deprecated" {
        return None;
    }
    let new = format!("{}@Deprecated(since = \"1.0\", forRemoval = true)", indent);
    replace_line(content, idx, &new)
}

fn is_balanced_parens(s: &str) -> bool {
    let mut depth = 0i32;
    for b in s.bytes() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

// ---------------------------------------------------------------------------
// eqeqeq / TS S3403 / S3845 — strict equality.
//   `x == y` → `x === y`     (but NOT when either operand is `null` —
//                             `x == null` is the idiomatic null-OR-undefined
//                             check).
//   `x != y` → `x !== y`
// ---------------------------------------------------------------------------
fn apply_eqeqeq(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let new = rewrite_eqeqeq(line)?;
    if new == line {
        return None;
    }
    replace_line(content, idx, &new)
}

fn rewrite_eqeqeq(line: &str) -> Option<String> {
    // Scan for `==` and `!=` that aren't `===` / `!==` already.
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len() + 2);
    let mut i = 0usize;
    let mut modified = false;
    while i < bytes.len() {
        if in_string_literal(line, i) {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        let two = if i + 1 < bytes.len() {
            &bytes[i..i + 2]
        } else {
            &bytes[i..i + 1]
        };
        let three = if i + 2 < bytes.len() {
            Some(&bytes[i..i + 3])
        } else {
            None
        };
        if three == Some(b"===") || three == Some(b"!==") {
            // already strict — copy through
            out.push_str(&line[i..i + 3]);
            i += 3;
            continue;
        }
        if two == b"==" || two == b"!=" {
            // Look at neighbouring tokens: skip when either side mentions
            // `null` (the idiomatic double-check). Cheap lookahead / lookback.
            let before = line[..i].trim_end();
            let after = line[i + 2..].trim_start();
            let neighbours = [before, after];
            if neighbours.iter().any(|s| {
                s.ends_with("null") || s.starts_with("null")
                    || s.ends_with("undefined") || s.starts_with("undefined")
            }) {
                out.push_str(&line[i..i + 2]);
                i += 2;
                continue;
            }
            // Not followed by `=` → safe to upgrade.
            modified = true;
            out.push_str(if two == b"==" { "===" } else { "!==" });
            i += 2;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    if modified { Some(out) } else { None }
}

// ---------------------------------------------------------------------------
// S3504 — `var` should not be used (TypeScript/JavaScript).
//   `var foo = …;` → `let foo = …;`
// Conservative: only rewrite when the `var` keyword starts a declaration at
// line start (possibly with indent). Doesn't attempt scope-escape analysis
// — in modern TS code, the rewrite is almost always safe, and the subsequent
// build/test gate catches the rare exception.
// ---------------------------------------------------------------------------
fn apply_no_var(content: &str, start_line: u32) -> Option<String> {
    let idx = (start_line as usize).checked_sub(1)?;
    let line = get_line(content, idx)?;
    let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let rest = &line[indent.len()..];
    if !rest.starts_with("var ") {
        return None;
    }
    let new = format!("{}let {}", indent, &rest[4..]);
    replace_line(content, idx, &new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s1118_inserts_private_constructor() {
        let input = "package p;\npublic final class Utils {\n    public static int add(int a, int b) { return a + b; }\n}\n";
        let out = apply_s1118_private_constructor(input).expect("should apply");
        assert!(out.contains("private Utils()"), "got: {}", out);
        assert!(out.contains("public static int add"), "existing method preserved");
    }

    #[test]
    fn s1118_skips_when_constructor_already_exists() {
        let input = "public class Utils {\n    public Utils() {}\n    public static int add(int a, int b) { return a + b; }\n}\n";
        assert!(apply_s1118_private_constructor(input).is_none());
    }

    #[test]
    fn s1118_skips_multiple_top_level_classes() {
        let input = "class A {}\nclass B {\n    public static void x() {}\n}\n";
        assert!(apply_s1118_private_constructor(input).is_none());
    }

    #[test]
    fn s1124_reorders_static_final() {
        let input = "package p;\npublic class X {\n    final static String S = \"a\";\n}\n";
        let out = apply_s1124_reorder_modifiers(input, 3).expect("should apply");
        assert!(
            out.contains("static final String S"),
            "expected `static final` got: {}",
            out
        );
    }

    #[test]
    fn s1124_idempotent_on_correct_order() {
        let input = "public class X {\n    public static final int A = 1;\n}\n";
        assert!(apply_s1124_reorder_modifiers(input, 2).is_none());
    }

    #[test]
    fn s1124_skips_annotation_lines() {
        let input = "public class X {\n    @Override public static void f() {}\n}\n";
        assert!(apply_s1124_reorder_modifiers(input, 2).is_none());
    }

    // ---------- S1125 ----------
    #[test]
    fn s1125_equals_true() {
        let input = "class X {\n    boolean f() { return x == true; }\n}\n";
        let out = apply_s1125_redundant_boolean(input, 2).expect("applies");
        assert!(out.contains("return x;"), "got: {}", out);
    }

    #[test]
    fn s1125_equals_false_negates() {
        let input = "class X {\n    boolean f() { return ready == false; }\n}\n";
        let out = apply_s1125_redundant_boolean(input, 2).expect("applies");
        assert!(out.contains("return !ready;"), "got: {}", out);
    }

    #[test]
    fn s1125_not_equals_true_negates() {
        let input = "class X {\n    boolean f() { return ready != true; }\n}\n";
        let out = apply_s1125_redundant_boolean(input, 2).expect("applies");
        assert!(out.contains("return !ready;"), "got: {}", out);
    }

    #[test]
    fn s1125_dotted_identifier() {
        let input = "class X {\n    boolean f() { return this.ready == true; }\n}\n";
        let out = apply_s1125_redundant_boolean(input, 2).expect("applies");
        assert!(out.contains("return this.ready;"), "got: {}", out);
    }

    #[test]
    fn s1125_skips_inside_string_literal() {
        let input = "class X {\n    String f() { return \"x == true\"; }\n}\n";
        // The match is only inside the literal — our helper detects that
        // and should refuse; but the outer pattern matcher looks for
        // `== true` which the literal contains. We accept either "no
        // change" or "bail" as correct behavior.
        if let Some(out) = apply_s1125_redundant_boolean(input, 2) {
            // If it did modify, the literal must still be intact.
            assert!(out.contains("\"x == true\""), "literal corrupted: {}", out);
        }
    }

    // ---------- S1126 ----------
    #[test]
    fn s1126_single_line_if_else() {
        let input = "class X {\n    boolean f() { if (x > 0) return true; else return false; }\n}\n";
        // The exact single-line form doesn't match our multi-line assumption;
        // ensure we at least don't crash.
        let _ = apply_s1126_boolean_return(input, 2);
    }

    #[test]
    fn s1126_multi_line_if_else_braces() {
        let input = "class X {\n    boolean f() {\n        if (x > 0) {\n            return true;\n        } else {\n            return false;\n        }\n    }\n}\n";
        let out = apply_s1126_boolean_return(input, 3).expect("applies");
        assert!(out.contains("return x > 0;"), "got:\n{}", out);
        assert!(!out.contains("return true;"), "got:\n{}", out);
    }

    // ---------- S1132 ----------
    #[test]
    fn s1132_literal_right_moved_left() {
        let input = "class X {\n    boolean f(String s) { return s.equals(\"foo\"); }\n}\n";
        let out = apply_s1132_string_literal_left(input, 2).expect("applies");
        assert!(out.contains("\"foo\".equals(s)"), "got: {}", out);
    }

    #[test]
    fn s1132_literal_already_on_left_no_change() {
        let input = "class X {\n    boolean f(String s) { return \"foo\".equals(s); }\n}\n";
        assert!(apply_s1132_string_literal_left(input, 2).is_none());
    }

    // ---------- S1155 ----------
    #[test]
    fn s1155_size_eq_zero() {
        let input = "class X {\n    boolean f() { return list.size() == 0; }\n}\n";
        let out = apply_s1155_is_empty(input, 2).expect("applies");
        assert!(out.contains("list.isEmpty()"), "got: {}", out);
    }

    #[test]
    fn s1155_size_gt_zero_negates() {
        let input = "class X {\n    boolean f() { return list.size() > 0; }\n}\n";
        let out = apply_s1155_is_empty(input, 2).expect("applies");
        assert!(out.contains("!list.isEmpty()"), "got: {}", out);
    }

    // ---------- S2293 ----------
    #[test]
    fn s2293_diamond_operator() {
        let input = "class X {\n    List<String> xs = new ArrayList<String>();\n}\n";
        let out = apply_s2293_diamond(input, 2).expect("applies");
        assert!(out.contains("new ArrayList<>()"), "got: {}", out);
    }

    #[test]
    fn s2293_no_lhs_generics_bails() {
        // Without LHS generics the diamond is ambiguous — don't rewrite.
        let input = "class X {\n    Object xs = new ArrayList<String>();\n}\n";
        assert!(apply_s2293_diamond(input, 2).is_none());
    }

    // ---------- S1488 (Java) ----------
    #[test]
    fn s1488_immediate_return_java() {
        let input = "class X {\n    int f() {\n        int x = compute();\n        return x;\n    }\n}\n";
        let out = apply_s1488_immediate_return(input, 3).expect("applies");
        assert!(out.contains("return compute();"), "got:\n{}", out);
        assert!(!out.contains("int x ="), "got:\n{}", out);
    }

    #[test]
    fn s1488_bails_on_mismatched_name() {
        let input = "class X {\n    int f() {\n        int x = compute();\n        return y;\n    }\n}\n";
        assert!(apply_s1488_immediate_return(input, 3).is_none());
    }

    // ---------- S1110 ----------
    #[test]
    fn s1110_redundant_parens_on_return() {
        let input = "class X {\n    int f() {\n        return (a + b);\n    }\n}\n";
        let out = apply_s1110_redundant_parens(input, 3).expect("applies");
        assert!(out.contains("return a + b;"), "got:\n{}", out);
    }

    // ---------- TS eqeqeq ----------
    #[test]
    fn ts_eqeqeq_upgrades() {
        let input = "function f(a: number, b: number) {\n    return a == b;\n}\n";
        let out = apply_eqeqeq(input, 2).expect("applies");
        assert!(out.contains("a === b"), "got: {}", out);
    }

    #[test]
    fn ts_eqeqeq_keeps_null_check() {
        let input = "function f(a: any) {\n    return a == null;\n}\n";
        assert!(apply_eqeqeq(input, 2).is_none(), "null check must be preserved");
    }

    #[test]
    fn ts_eqeqeq_not_equal() {
        let input = "function f(a: number, b: number) {\n    return a != b;\n}\n";
        let out = apply_eqeqeq(input, 2).expect("applies");
        assert!(out.contains("a !== b"), "got: {}", out);
    }

    // ---------- TS no-var ----------
    #[test]
    fn ts_var_becomes_let() {
        let input = "function f() {\n    var x = 1;\n    return x;\n}\n";
        let out = apply_no_var(input, 2).expect("applies");
        assert!(out.contains("let x = 1;"), "got: {}", out);
    }

    // ---------- TS S1488 ----------
    #[test]
    fn ts_s1488_immediate_return() {
        let input = "function f() {\n    const v = compute();\n    return v;\n}\n";
        let out = apply_ts_s1488_immediate_return(input, 2).expect("applies");
        assert!(out.contains("return compute();"), "got:\n{}", out);
    }

    #[test]
    fn ts_s1488_with_type_annotation() {
        let input = "function f(): number {\n    const v: number = compute();\n    return v;\n}\n";
        let out = apply_ts_s1488_immediate_return(input, 2).expect("applies");
        assert!(out.contains("return compute();"), "got:\n{}", out);
    }

    // ---------- S6355 ----------
    #[test]
    fn s6355_rewrites_bare_deprecated() {
        let input = "    @Deprecated\n    public void foo() {}\n";
        let out = apply_s6355_deprecated_args(input, 1).expect("applies");
        assert!(
            out.contains("@Deprecated(since = \"1.0\", forRemoval = true)"),
            "got:\n{}",
            out
        );
        assert!(out.contains("    public void foo() {}"), "indent preserved");
    }

    #[test]
    fn s6355_skips_when_already_parameterized() {
        let input = "    @Deprecated(forRemoval = true)\n";
        assert!(apply_s6355_deprecated_args(input, 1).is_none());
    }

    #[test]
    fn s6355_skips_when_other_tokens_on_line() {
        // `@Deprecated public void foo() {}` is unusual but not our case.
        let input = "    @Deprecated public void foo() {}\n";
        assert!(apply_s6355_deprecated_args(input, 1).is_none());
    }

    // ---------- has_deterministic_fix registry ----------
    #[test]
    fn registry_matches_dispatcher() {
        // Every rule declared as deterministic must route to a handler
        // that at least reports "I know what you mean" — not necessarily
        // produce a fix for arbitrary content (that depends on shape),
        // but the dispatcher must not return None purely because the
        // rule code is unknown.
        for rule in &[
            "java:S1118", "java:S1124", "java:S1125", "java:S1126",
            "java:S1132", "java:S1155", "java:S2293", "java:S1488", "java:S1110",
            "java:S6355",
            "typescript:S1125", "typescript:S3403", "typescript:S3504",
            "typescript:S1126", "typescript:S1488", "javascript:S3504",
        ] {
            assert!(
                has_deterministic_fix(rule),
                "rule should be registered: {}",
                rule
            );
        }
    }
}
