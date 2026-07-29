use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    style::Print,
    terminal::{self, ClearType},
};

use crossterm::style::{Color, ResetColor, SetBackgroundColor, SetForegroundColor};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher};

use syntect::{
    easy::HighlightLines,
    highlighting::ThemeSet,
    parsing::SyntaxSet,
};

use std::io::{stdout, Write};
use std::path::PathBuf;
use std::collections::HashSet;

//test 

#[derive(PartialEq)]
enum Mode {
    Normal,
    Insert,
    Command,
    SavePrompt,
    OpenPrompt,
    CdPrompt,
    DirBrowse,
    NewFilePrompt,
    NewFolderPrompt,
    StatusMsg,
}

/// Returned by handle_command so the main loop knows whether to quit.
enum CommandResult {
    Quit,
    Stay,
}

/// What Mode::DirBrowse is being used for — changes what Enter does on a
/// file, what the footer hint says, and what the '.'  and '/' keys do.
#[derive(PartialEq, Clone, Copy)]
enum BrowsePurpose {
    Cd,
    Open,
}

fn main() -> std::io::Result<()> {
    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, terminal::EnterAlternateScreen)?;

    let result = run(&mut stdout);

    execute!(stdout, terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

    result
}

fn buffer_to_string(lines: &[Vec<char>]) -> String {
    lines
        .iter()
        .map(|l| l.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn native_save_dialog(suggested: Option<&str>) -> Option<PathBuf> {
    #[cfg(feature = "native-dialog")]
    {
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Text files", &["txt", "rs", "md"])
            .add_filter("All files", &["*"]);
        if let Some(name) = suggested {
            dialog = dialog.set_file_name(name);
        }
        return dialog.save_file();
    }
    #[cfg(not(feature = "native-dialog"))]
    {
        let _ = suggested;
        None
    }
}

// ── Autocomplete ─────────────────────────────────────────────────────────
//
// Lightweight, Vim-`<C-n>`-style keyword completion: candidates come from
// (1) identifier-like words already used elsewhere in the buffer, and
// (2) a small built-in keyword list picked by the open file's extension.
// No language server, no parsing — just word scanning + a static table.

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
    "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "union", "yield", "String", "Vec", "Option",
    "Result", "Some", "None", "Ok", "Err", "Box", "Rc", "Arc", "i8", "i16", "i32", "i64", "isize",
    "u8", "u16", "u32", "u64", "usize", "f32", "f64", "bool", "char", "str", "println!",
    "format!", "vec!", "panic!", "assert!", "derive",
];

const PYTHON_KEYWORDS: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self", "print", "len", "range", "str", "int", "float", "bool", "list",
    "dict", "set", "tuple", "None",
];

const JS_KEYWORDS: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
    "default", "delete", "do", "else", "export", "extends", "false", "finally", "for", "function",
    "if", "import", "in", "instanceof", "let", "new", "null", "of", "return", "static", "super",
    "switch", "this", "throw", "true", "try", "typeof", "var", "void", "while", "with", "yield",
    "interface", "type", "implements", "enum", "namespace", "console", "document", "window",
];

const C_KEYWORDS: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else",
    "enum", "extern", "float", "for", "goto", "if", "int", "long", "register", "return", "short",
    "signed", "sizeof", "static", "struct", "switch", "typedef", "union", "unsigned", "void",
    "volatile", "while", "include", "define", "NULL", "printf", "scanf", "malloc", "free",
];

const CPP_KEYWORDS: &[&str] = &[
    "class", "namespace", "template", "typename", "public", "private", "protected", "virtual",
    "override", "friend", "new", "delete", "this", "try", "catch", "throw", "using", "auto",
    "const", "static", "struct", "enum", "return", "if", "else", "for", "while", "switch",
    "case", "break", "continue", "nullptr", "true", "false", "bool", "int", "long", "double",
    "float", "char", "void", "std", "vector", "string", "cout", "cin", "endl",
];

const GO_KEYWORDS: &[&str] = &[
    "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough",
    "for", "func", "go", "goto", "if", "import", "interface", "map", "package", "range",
    "return", "select", "struct", "switch", "type", "var", "true", "false", "nil", "string",
    "int", "int64", "float64", "bool", "error", "fmt", "println",
];

/// Pick a keyword table by file extension. Unknown/missing extensions fall
/// back to an empty slice, so completion still works (buffer words only).
fn keywords_for_extension(ext: &str) -> &'static [&'static str] {
    match ext {
        "rs" => RUST_KEYWORDS,
        "py" => PYTHON_KEYWORDS,
        "js" | "jsx" | "ts" | "tsx" | "mjs" => JS_KEYWORDS,
        "c" | "h" => C_KEYWORDS,
        "cpp" | "cc" | "cxx" | "hpp" => CPP_KEYWORDS,
        "go" => GO_KEYWORDS,
        _ => &[],
    }
}

/// Scan backward from `cursor_x` over identifier characters (alnum + '_')
/// to find where the word currently being typed starts. Returns the start
/// column and the prefix text itself.
fn word_prefix_at_cursor(line: &[char], cursor_x: usize) -> (usize, String) {
    let end = cursor_x.min(line.len());
    let mut start = end;
    while start > 0 && (line[start - 1].is_alphanumeric() || line[start - 1] == '_') {
        start -= 1;
    }
    (start, line[start..end].iter().collect())
}

/// Collect every distinct identifier-like word (2+ chars) used anywhere in
/// the buffer.
fn collect_buffer_words(lines: &[Vec<char>]) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();
    for line in lines {
        let mut current = String::new();
        for &c in line {
            if c.is_alphanumeric() || c == '_' {
                current.push(c);
            } else if current.len() > 1 {
                set.insert(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        if current.len() > 1 {
            set.insert(current);
        }
    }
    let mut words: Vec<String> = set.into_iter().collect();
    words.sort();
    words
}

/// Build the suggestion list for `prefix`: matching buffer words first
/// (most contextually relevant), then matching language keywords, deduped,
/// capped at 20 entries so the popup stays small.
/// Build the suggestion list for `prefix`: gather candidates from buffer
/// words and the language's keyword table, then fuzzy-rank them against
/// what's been typed so far (same matcher Helix uses for its pickers),
/// capped at 20 entries so the popup stays small.
fn compute_suggestions(prefix: &str, lines: &[Vec<char>], file_path: &Option<PathBuf>) -> Vec<String> {
    if prefix.is_empty() {
        return Vec::new();
    }

    let ext = file_path
        .as_ref()
        .and_then(|p| p.extension())
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let keywords = keywords_for_extension(ext);

    let mut seen: HashSet<String> = HashSet::new();
    let mut candidates: Vec<String> = Vec::new();

    for w in collect_buffer_words(lines) {
        if w != prefix && seen.insert(w.clone()) {
            candidates.push(w);
        }
    }
    for &kw in keywords {
        if kw != prefix && seen.insert(kw.to_string()) {
            candidates.push(kw.to_string());
        }
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut scored = Atom::new(
        prefix,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    )
    .match_list(candidates, &mut matcher);

    // Highest score (best match) first.
    scored.sort_by(|a, b| b.1.cmp(&a.1));

    scored.into_iter().map(|(s, _)| s).take(20).collect()
}

/// Splice the un-typed remainder of `suggestion` into `line` at the cursor,
/// then move the cursor past the inserted text.
fn accept_autocomplete(line: &mut Vec<char>, cursor_x: &mut usize, suggestion: &str) {
    let (start, prefix) = word_prefix_at_cursor(line, *cursor_x);
    let suffix: Vec<char> = suggestion.chars().skip(prefix.chars().count()).collect();
    for (i, c) in suffix.iter().enumerate() {
        line.insert(*cursor_x + i, *c);
    }
    *cursor_x = start + suggestion.chars().count();
}

// ── Auto-closing pairs (VSCode-style) ───────────────────────────────────

/// How many columns a Tab press advances (to the next multiple of this).
const TAB_WIDTH: usize = 4;

/// The closing character for an opening bracket/quote, if `c` is one.
fn matching_closer(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

/// True for the three bracket closers (quotes are handled separately,
/// since open == close for them).
fn is_bracket_closer(c: char) -> bool {
    matches!(c, ')' | ']' | '}')
}

fn write_file(path: &PathBuf, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content)
}

/// Read `dir` and return (name, is_dir) pairs, directories first, then
/// alphabetical (case-insensitive) within each group. Unreadable or missing
/// directories just come back empty rather than erroring.
fn list_dir_entries(dir: &PathBuf) -> Vec<(String, bool)> {
    let mut entries: Vec<(String, bool)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    (name, is_dir)
                })
                .collect()
        })
        .unwrap_or_default();

    entries.sort_by(|a, b| {
        b.1.cmp(&a.1) // directories (true) sort before files (false)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    entries
}

/// Resolve `input` against `current_dir` (or treat it as absolute if it is),
/// returning the canonical path only if it exists and is a directory.
fn resolve_dir(current_dir: &PathBuf, input: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(input);
    let candidate = if candidate.is_absolute() {
        candidate
    } else {
        current_dir.join(candidate)
    };

    match candidate.canonicalize() {
        Ok(p) if p.is_dir() => Some(p),
        _ => None,
    }
}

/// Load `path` into the buffer, replacing whatever is currently open.
/// Shared by :op (typed path or inline argument) and by picking a file in
/// the browser.
fn do_open(
    path: &PathBuf,
    mode: &mut Mode,
    dirty: &mut bool,
    file_path: &mut Option<PathBuf>,
    lines: &mut Vec<Vec<char>>,
    cursor_x: &mut usize,
    cursor_y: &mut usize,
    status_msg: &mut String,
) {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            *lines = contents.lines().map(|l| l.chars().collect()).collect();
            if lines.is_empty() {
                lines.push(Vec::new());
            }
            *cursor_x = 0;
            *cursor_y = 0;
            *dirty = false;
            *file_path = Some(path.clone());
            *status_msg = format!("Opened: {}", path.display());
        }
        Err(e) => {
            *status_msg = format!("Error opening '{}': {}", path.display(), e);
        }
    }
    *mode = Mode::StatusMsg;
}

/// Resolve a user-typed path/name against `current_dir` unless it's already absolute.
fn resolve_against(current_dir: &PathBuf, input: &str) -> PathBuf {
    let candidate = PathBuf::from(input);
    if candidate.is_absolute() {
        candidate
    } else {
        current_dir.join(candidate)
    }
}

/// Open the directory browser (Mode::DirBrowse) rooted at `current_dir`,
/// tagged with `purpose` so it knows whether it's picking a directory (:cd)
/// or a file (:op).
fn start_browsing(
    purpose: BrowsePurpose,
    current_dir: &PathBuf,
    browse_dir: &mut PathBuf,
    browse_entries: &mut Vec<(String, bool)>,
    browse_selected: &mut usize,
    browse_purpose: &mut BrowsePurpose,
    mode: &mut Mode,
) {
    *browse_purpose = purpose;
    *browse_dir = current_dir.clone();
    *browse_entries = list_dir_entries(browse_dir);
    *browse_selected = 0;
    *mode = Mode::DirBrowse;
}

/// `:cd <target>` — change the editor's "current directory" used as the base
/// for :e and :a. Does not touch the open buffer.
fn do_cd(current_dir: &mut PathBuf, target: &str, status_msg: &mut String, mode: &mut Mode) {
    match resolve_dir(current_dir, target) {
        Some(p) => {
            *status_msg = format!("cd: {}", p.display());
            *current_dir = p;
        }
        None => {
            *status_msg = format!("cd: not a directory: {}", target);
        }
    }
    *mode = Mode::StatusMsg;
}

/// `:e <name>` — create a new (empty) file inside `current_dir` and open it
/// into the buffer, replacing whatever is currently loaded.
fn do_new_file(
    current_dir: &PathBuf,
    name: &str,
    mode: &mut Mode,
    dirty: &mut bool,
    file_path: &mut Option<PathBuf>,
    lines: &mut Vec<Vec<char>>,
    cursor_x: &mut usize,
    cursor_y: &mut usize,
    status_msg: &mut String,
) {
    let path = current_dir.join(name);

    if path.exists() {
        *status_msg = format!("File already exists: {}", path.display());
        *mode = Mode::StatusMsg;
        return;
    }

    // Make sure any intermediate directories in `name` (e.g. "sub/new.rs") exist.
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            *status_msg = format!("Error creating file: {}", e);
            *mode = Mode::StatusMsg;
            return;
        }
    }

    match std::fs::File::create(&path) {
        Ok(_) => {
            *lines = vec![Vec::new()];
            *cursor_x = 0;
            *cursor_y = 0;
            *dirty = false;
            *file_path = Some(path.clone());
            *status_msg = format!("Created and opened: {}", path.display());
        }
        Err(e) => {
            *status_msg = format!("Error creating file: {}", e);
        }
    }
    *mode = Mode::StatusMsg;
}

/// `:a <name>` — create a new folder inside `current_dir`, then `cd` into it
/// so a following `:a` or `:e` operates inside the freshly created folder.
fn do_new_folder(current_dir: &mut PathBuf, name: &str, status_msg: &mut String, mode: &mut Mode) {
    let path = current_dir.join(name);

    match std::fs::create_dir_all(&path) {
        Ok(_) => match path.canonicalize() {
            Ok(p) => {
                *status_msg = format!("Created folder, now in: {}", p.display());
                *current_dir = p;
            }
            Err(e) => {
                *status_msg = format!("Created folder but couldn't cd into it: {}", e);
            }
        },
        Err(e) => {
            *status_msg = format!("Error creating folder: {}", e);
        }
    }
    *mode = Mode::StatusMsg;
}

/// Shared save logic. Returns true if the file was successfully written
/// (so :wq knows it can quit).
fn do_save(
    mode: &mut Mode,
    dirty: &mut bool,
    file_path: &mut Option<PathBuf>,
    save_prompt_buf: &mut String,
    status_msg: &mut String,
    lines: &[Vec<char>],
) -> bool {
    let content = buffer_to_string(lines);

    if let Some(path) = file_path.clone() {
        match write_file(&path, &content) {
            Ok(_) => {
                *dirty = false;
                *status_msg = format!("Saved: {}", path.display());
                *mode = Mode::StatusMsg;
                true
            }
            Err(e) => {
                *status_msg = format!("Error saving: {}", e);
                *mode = Mode::StatusMsg;
                false
            }
        }
    } else if let Some(path) = native_save_dialog(None) {
        match write_file(&path, &content) {
            Ok(_) => {
                *dirty = false;
                *status_msg = format!("Saved: {}", path.display());
                *file_path = Some(path);
                *mode = Mode::StatusMsg;
                true
            }
            Err(e) => {
                *status_msg = format!("Error saving: {}", e);
                *mode = Mode::StatusMsg;
                false
            }
        }
    } else {
        // Fall back to in-terminal filename prompt.
        save_prompt_buf.clear();
        *mode = Mode::SavePrompt;
        false
    }
}

/// Handle a completed command string (the text after `:` + Enter).
fn handle_command(
    cmd: &str,
    mode: &mut Mode,
    dirty: &mut bool,
    file_path: &mut Option<PathBuf>,
    save_prompt_buf: &mut String,
    new_file_prompt_buf: &mut String,
    new_folder_prompt_buf: &mut String,
    current_dir: &mut PathBuf,
    browse_dir: &mut PathBuf,
    browse_entries: &mut Vec<(String, bool)>,
    browse_selected: &mut usize,
    browse_purpose: &mut BrowsePurpose,
    status_msg: &mut String,
    lines: &mut Vec<Vec<char>>,
    cursor_x: &mut usize,
    cursor_y: &mut usize,
) -> CommandResult {
    // Split "cd some/dir" into ("cd", Some("some/dir")); "cd" alone -> ("cd", None).
    let mut parts = cmd.splitn(2, ' ');
    let head = parts.next().unwrap_or("");
    let arg = parts.next().map(str::trim).filter(|s| !s.is_empty());

    match head {
        "q" => {
            if *dirty {
                *status_msg =
                    "Unsaved changes! Use :w to save, or :q! to force quit.".to_string();
                *mode = Mode::StatusMsg;
                CommandResult::Stay
            } else {
                CommandResult::Quit
            }
        }
        "q!" => CommandResult::Quit,
        "w" => {
            do_save(mode, dirty, file_path, save_prompt_buf, status_msg, lines);
            CommandResult::Stay
        }
        "wq" => {
            let saved = do_save(mode, dirty, file_path, save_prompt_buf, status_msg, lines);
            if saved {
                CommandResult::Quit
            } else {
                CommandResult::Stay
            }
        }
        "op" => {
            match arg {
                // ":op some/file.rs" — open it directly.
                Some(name) => {
                    let path = resolve_against(current_dir, name);
                    do_open(&path, mode, dirty, file_path, lines, cursor_x, cursor_y, status_msg);
                }
                // ":op" alone — open the browser to pick a file.
                None => start_browsing(
                    BrowsePurpose::Open,
                    current_dir,
                    browse_dir,
                    browse_entries,
                    browse_selected,
                    browse_purpose,
                    mode,
                ),
            }
            CommandResult::Stay
        }
        "cd" => {
            match arg {
                // ":cd some/dir" — jump straight there.
                Some(target) => do_cd(current_dir, target, status_msg, mode),
                // ":cd" alone — open the browser to pick a directory.
                None => start_browsing(
                    BrowsePurpose::Cd,
                    current_dir,
                    browse_dir,
                    browse_entries,
                    browse_selected,
                    browse_purpose,
                    mode,
                ),
            }
            CommandResult::Stay
        }
        "e" => {
            match arg {
                Some(name) => do_new_file(
                    current_dir,
                    name,
                    mode,
                    dirty,
                    file_path,
                    lines,
                    cursor_x,
                    cursor_y,
                    status_msg,
                ),
                None => {
                    new_file_prompt_buf.clear();
                    *mode = Mode::NewFilePrompt;
                }
            }
            CommandResult::Stay
        }
        "a" => {
            match arg {
                Some(name) => do_new_folder(current_dir, name, status_msg, mode),
                None => {
                    new_folder_prompt_buf.clear();
                    *mode = Mode::NewFolderPrompt;
                }
            }
            CommandResult::Stay
        }
        _ => {
            *mode = Mode::Normal;
            CommandResult::Stay
        }
    }
}

// ── Gutter & status bar styling ─────────────────────────────────────────

/// Width, in columns, of the line-number field alone (right-aligned digits).
fn gutter_digits(total_lines: usize) -> usize {
    total_lines.to_string().len().max(3)
}

/// Full gutter width: the digits, a space, a "│" separator, and a trailing
/// space before the text starts.
fn gutter_width(total_lines: usize) -> usize {
    gutter_digits(total_lines) + 3
}

/// Print one line-number cell ("{n} │ "). The current line's number is
/// picked out in a brighter color than the rest.
fn print_gutter(
    stdout: &mut impl Write,
    line_num: usize,
    digits: usize,
    is_current: bool,
) -> std::io::Result<()> {
    let num_color = if is_current { Color::Yellow } else { Color::DarkGrey };
    execute!(
        stdout,
        SetForegroundColor(num_color),
        Print(format!("{:>width$}", line_num, width = digits)),
        SetForegroundColor(Color::DarkGrey),
        Print(" │ "),
        ResetColor
    )
}

/// Fixed width of the mode "badge" at the start of the status bar
/// (including its padding), plus the one separating space after it.
const STATUS_BADGE_WIDTH: usize = 10;
const STATUS_TEXT_START: usize = STATUS_BADGE_WIDTH + 1;

/// Draw the status bar as a colored band spanning the full width, with a
/// mode badge on the left (colored per-mode), free text after it, and an
/// optional right-aligned segment (used for Ln/Col/line-count).
fn render_status_bar(
    stdout: &mut impl Write,
    cols: usize,
    row: u16,
    label: &str,
    badge_color: Color,
    left_text: &str,
    right_text: &str,
) -> std::io::Result<()> {
    let band_bg = Color::DarkGrey;

    // Base band across the whole row.
    execute!(stdout, cursor::MoveTo(0, row))?;
    execute!(stdout, SetBackgroundColor(band_bg), SetForegroundColor(Color::White))?;
    execute!(stdout, Print(" ".repeat(cols)))?;

    // Badge.
    execute!(stdout, cursor::MoveTo(0, row))?;
    execute!(stdout, SetBackgroundColor(badge_color), SetForegroundColor(Color::Black))?;
    execute!(stdout, Print(format!(" {:^8} ", label)))?;

    // Body text.
    execute!(stdout, SetBackgroundColor(band_bg), SetForegroundColor(Color::White))?;
    let available = cols.saturating_sub(STATUS_TEXT_START);
    let left_clip: String = left_text.chars().take(available).collect();
    execute!(stdout, Print(" "), Print(&left_clip))?;

    // Right-aligned segment (cursor position, line count, etc.), if it fits.
    if !right_text.is_empty() {
        let right_len = right_text.chars().count();
        let right_col = cols.saturating_sub(right_len + 1);
        let left_end = STATUS_TEXT_START + left_clip.chars().count();
        if right_col > left_end {
            execute!(stdout, cursor::MoveTo(right_col as u16, row))?;
            execute!(stdout, Print(right_text))?;
        }
    }

    execute!(stdout, ResetColor)?;
    Ok(())
}

/// Draw the autocomplete suggestion list just below the cursor, clipped to
/// stay on-screen and to leave the status bar row free.
fn render_autocomplete_popup(
    stdout: &mut impl Write,
    suggestions: &[String],
    selected: usize,
    cursor_col: usize,
    cursor_row: usize,
    rows: usize,
    cols: usize,
) -> std::io::Result<()> {
    if suggestions.is_empty() {
        return Ok(());
    }

    let popup_col = cursor_col.min(cols.saturating_sub(1));
    let max_width = cols.saturating_sub(popup_col).max(4);
    let width = suggestions
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(4)
        .min(max_width)
        .max(4);

    // Leave the status bar row free; stop the popup before it.
    let available_rows = rows.saturating_sub(1).saturating_sub(cursor_row + 1);
    if available_rows == 0 {
        return Ok(());
    }
    let visible_count = suggestions.len().min(available_rows).min(10);

    for (i, s) in suggestions.iter().take(visible_count).enumerate() {
        let screen_row = (cursor_row + 1 + i) as u16;
        execute!(stdout, cursor::MoveTo(popup_col as u16, screen_row))?;

        let text: String = s.chars().take(width).collect();
        let padded = format!("{:<width$}", text, width = width);

        if i == selected {
            execute!(stdout, SetForegroundColor(Color::Black), SetBackgroundColor(Color::White))?;
        } else {
            execute!(stdout, SetForegroundColor(Color::Black), SetBackgroundColor(Color::Grey))?;
        }
        execute!(stdout, Print(padded))?;
        execute!(stdout, ResetColor)?;
    }

    Ok(())
}

fn render_dir_browser(
    stdout: &mut impl Write,
    browse_dir: &PathBuf,
    entries: &[(String, bool)],
    selected: usize,
    purpose: BrowsePurpose,
    rows: usize,
    cols: usize,
) -> std::io::Result<()> {
    execute!(stdout, cursor::Hide)?;
    execute!(stdout, terminal::Clear(ClearType::All))?;

    // Header: what we're doing, and where we're currently browsing.
    execute!(stdout, cursor::MoveTo(0, 0))?;
    let label = match purpose {
        BrowsePurpose::Cd => "CD",
        BrowsePurpose::Open => "OPEN",
    };
    let header: String = format!("-- {} -- {}", label, browse_dir.display())
        .chars()
        .take(cols)
        .collect();
    execute!(stdout, SetForegroundColor(Color::Yellow), Print(header), ResetColor)?;

    // Entry list, scrolled so the selection is always visible.
    let list_rows = rows.saturating_sub(2); // 1 header row + 1 footer row
    let visible_start = if selected >= list_rows {
        selected + 1 - list_rows
    } else {
        0
    };

    if entries.is_empty() {
        execute!(stdout, cursor::MoveTo(0, 1))?;
        execute!(stdout, Print("(empty directory)"))?;
    }

    for (i, (name, is_dir)) in entries.iter().enumerate().skip(visible_start).take(list_rows) {
        let screen_row = (i - visible_start) as u16 + 1;
        execute!(stdout, cursor::MoveTo(0, screen_row))?;

        let marker = if i == selected { "> " } else { "  " };
        let label = if *is_dir { format!("{}/", name) } else { name.clone() };
        let line: String = format!("{}{}", marker, label).chars().take(cols).collect();

        if i == selected {
            execute!(stdout, SetForegroundColor(Color::Black), SetBackgroundColor(Color::White))?;
        } else if *is_dir {
            execute!(stdout, SetForegroundColor(Color::Cyan))?;
        } else {
            execute!(stdout, SetForegroundColor(Color::Grey))?;
        }

        execute!(stdout, Print(line))?;
        execute!(stdout, ResetColor)?;
    }

    // Footer: controls (slightly different depending on what we're picking).
    execute!(stdout, cursor::MoveTo(0, rows.saturating_sub(1) as u16))?;
    let footer = match purpose {
        BrowsePurpose::Cd => "j/k move   l/Enter open dir   h back   . select this dir   / type a path   Esc cancel",
        BrowsePurpose::Open => "j/k move   l/Enter open dir/file   h back   / type a path   Esc cancel",
    };
    execute!(stdout, Print(footer))?;

    stdout.flush()?;
    Ok(())
}

fn run(stdout: &mut impl Write) -> std::io::Result<()> {

    let ps = SyntaxSet::load_defaults_newlines();
    let ts = ThemeSet::load_defaults();

    let theme = &ts.themes["base16-ocean.dark"];

    let mut mode = Mode::Normal;
    let mut command_buffer = String::new();

    let mut lines: Vec<Vec<char>> = vec![Vec::new()];
    let mut row_offset: usize = 0;
    let mut col_offset: usize = 0;
    let mut cursor_x: usize = 0;
    let mut cursor_y: usize = 0;


    let mut dirty = false;
    let mut file_path: Option<PathBuf> = None;


    let mut save_prompt_buf = String::new();
    let mut open_prompt_buf = String::new();
    let mut cd_prompt_buf = String::new();
    let mut new_file_prompt_buf = String::new();
    let mut new_folder_prompt_buf = String::new();
    let mut status_msg = String::new();

    // Base directory used by :cd, :e, and :a. Defaults to wherever the
    // editor was launched from.
    let mut current_dir: PathBuf =
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // State for the :cd directory browser (Mode::DirBrowse).
    let mut browse_dir: PathBuf = current_dir.clone();
    let mut browse_entries: Vec<(String, bool)> = Vec::new();
    let mut browse_selected: usize = 0;
    let mut browse_purpose: BrowsePurpose = BrowsePurpose::Cd;

    // State for Insert-mode keyword autocomplete (Ctrl-N / Ctrl-P / Tab).
    let mut autocomplete_active = false;
    let mut autocomplete_suggestions: Vec<String> = Vec::new();
    let mut autocomplete_selected: usize = 0;

    'editor: loop {
        let (_cols, rows) = terminal::size()?;
        let rows_usize = rows as usize;
        let cols_usize = _cols as usize;

        let gutter_w = gutter_width(lines.len());
        let text_cols = cols_usize.saturating_sub(gutter_w).max(1);

        let scroll_margin = 5;

        let max_row_offset = lines.len().saturating_sub(rows_usize.saturating_sub(1));

        if cursor_y < row_offset + scroll_margin {
            row_offset = cursor_y.saturating_sub(scroll_margin);
        } else if cursor_y >= row_offset + rows_usize.saturating_sub(1).saturating_sub(scroll_margin) {
            row_offset = cursor_y.saturating_sub(rows_usize.saturating_sub(1).saturating_sub(scroll_margin));
        }

        if row_offset > max_row_offset {
            row_offset = max_row_offset;
        }

        let horizontal_margin = 8;

        if cursor_x < col_offset + horizontal_margin {
            col_offset = cursor_x.saturating_sub(horizontal_margin);
        } else if cursor_x >= col_offset + text_cols.saturating_sub(horizontal_margin) {
            col_offset = cursor_x.saturating_sub(text_cols.saturating_sub(horizontal_margin));
        }

        // ── Render ───────────────────────────────────────────────────────────
        if mode == Mode::DirBrowse {
            render_dir_browser(
                stdout,
                &browse_dir,
                &browse_entries,
                browse_selected,
                browse_purpose,
                rows_usize,
                cols_usize,
            )?;
        } else {
        execute!(stdout, cursor::Hide)?;

        execute!(stdout, terminal::Clear(ClearType::All))?;


        for (screen_y, line) in lines
            .iter()
            .skip(row_offset)
            .take(rows_usize.saturating_sub(1))
            .enumerate() {
                if screen_y >= rows_usize.saturating_sub(1) {
                    break;
                }

                let buffer_line_num = screen_y + row_offset;
                let is_current_line = buffer_line_num == cursor_y;

                let full_text: String = line.iter().collect();
                let visible_text: String = full_text
                    .chars()
                    .skip(col_offset)
                    .take(text_cols)
                    .collect();

                let syntax = if let Some(path) = &file_path {
                    path.extension()
                        .and_then(|s| s.to_str())
                        .and_then(|ext| ps.find_syntax_by_extension(ext))
                        .unwrap_or_else(|| ps.find_syntax_plain_text())
                } else {
                ps.find_syntax_plain_text()
            };

            let mut h = HighlightLines::new(syntax, theme);

            let ranges = h.highlight_line(&visible_text, &ps).unwrap_or_default();

            execute!(stdout, cursor::MoveTo(0, screen_y as u16))?;
            print_gutter(stdout, buffer_line_num + 1, gutter_digits(lines.len()), is_current_line)?;

            // Subtle full-width highlight on the line the cursor is on.
            if is_current_line {
                execute!(stdout, SetBackgroundColor(Color::DarkGrey))?;
            }

            for(style, piece) in ranges {
                let color = Color::Rgb {
                    r: style.foreground.r,
                    g: style.foreground.g,
                    b: style.foreground.b,
                };

                execute!(
                    stdout,
                    SetForegroundColor(color),
                    Print(piece)
                )?;
            }

            if is_current_line {
                // Pad out to the edge of the text area so the highlight
                // covers the whole line, not just where the text ends.
                let printed = visible_text.chars().count();
                let remaining = text_cols.saturating_sub(printed);
                if remaining > 0 {
                    execute!(stdout, Print(" ".repeat(remaining)))?;
                }
            }

            execute!(stdout, ResetColor)?;
        }

        // Tilde rows past the end of the buffer, like Vim's empty-line marker.
        let printed_rows = lines.len().saturating_sub(row_offset).min(rows_usize.saturating_sub(1));
        for r in printed_rows..rows_usize.saturating_sub(1) {
            execute!(stdout, cursor::MoveTo(0, r as u16))?;
            execute!(stdout, SetForegroundColor(Color::DarkGrey), Print("~"), ResetColor)?;
        }

        if mode == Mode::Insert && autocomplete_active {
            let cursor_screen_col = gutter_w + cursor_x.saturating_sub(col_offset);
            let cursor_screen_row = cursor_y.saturating_sub(row_offset);
            render_autocomplete_popup(
                stdout,
                &autocomplete_suggestions,
                autocomplete_selected,
                cursor_screen_col,
                cursor_screen_row,
                rows_usize,
                cols_usize,
            )?;
        }



        // Status bar
        {
            let pos_info = format!(
                "Ln {}, Col {} · {} lines",
                cursor_y + 1,
                cursor_x + 1,
                lines.len()
            );
            match mode {
                Mode::Normal => {
                    let fname = file_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "[No Name]".to_string());
                    let modified = if dirty { " [+]" } else { "" };
                    let left = format!("{}{}   {}", fname, modified, current_dir.display());
                    render_status_bar(stdout, cols_usize, rows - 1, "NORMAL", Color::Blue, &left, &pos_info)?;
                }
                Mode::Insert => {
                    let fname = file_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "[No Name]".to_string());
                    let modified = if dirty { " [+]" } else { "" };
                    let left = format!("{}{}", fname, modified);
                    render_status_bar(stdout, cols_usize, rows - 1, "INSERT", Color::Green, &left, &pos_info)?;
                }
                Mode::Command => {
                    let left = format!(":{}", command_buffer);
                    render_status_bar(stdout, cols_usize, rows - 1, "CMD", Color::DarkYellow, &left, "")?;
                }
                Mode::SavePrompt => {
                    let left = format!("Save as: {}", save_prompt_buf);
                    render_status_bar(stdout, cols_usize, rows - 1, "SAVE", Color::Magenta, &left, "")?;
                }
                Mode::OpenPrompt => {
                    let left = format!("Open: {}", open_prompt_buf);
                    render_status_bar(stdout, cols_usize, rows - 1, "OPEN", Color::Magenta, &left, "")?;
                }
                Mode::CdPrompt => {
                    let left = format!("cd: {}", cd_prompt_buf);
                    render_status_bar(stdout, cols_usize, rows - 1, "CD", Color::Magenta, &left, "")?;
                }
                Mode::NewFilePrompt => {
                    let left = format!("New file in {}: {}", current_dir.display(), new_file_prompt_buf);
                    render_status_bar(stdout, cols_usize, rows - 1, "NEW FILE", Color::Magenta, &left, "")?;
                }
                Mode::NewFolderPrompt => {
                    let left = format!("New folder in {}: {}", current_dir.display(), new_folder_prompt_buf);
                    render_status_bar(stdout, cols_usize, rows - 1, "NEW DIR", Color::Magenta, &left, "")?;
                }
                Mode::StatusMsg => {
                    render_status_bar(stdout, cols_usize, rows - 1, "INFO", Color::Cyan, &status_msg, "")?;
                }
                Mode::DirBrowse => unreachable!(),
            }
        }

        // Cursor
        match mode {
            Mode::Command => {
                let col = STATUS_TEXT_START as u16 + 1 + command_buffer.len() as u16;
                execute!(stdout, cursor::MoveTo(col, rows - 1))?;
            }
            Mode::SavePrompt => {
                let col = STATUS_TEXT_START as u16 + "Save as: ".len() as u16 + save_prompt_buf.len() as u16;
                execute!(stdout, cursor::MoveTo(col, rows - 1))?;
            }
            Mode::OpenPrompt => {
                let col = STATUS_TEXT_START as u16 + "Open: ".len() as u16 + open_prompt_buf.len() as u16;
                execute!(stdout, cursor::MoveTo(col, rows - 1))?;
            }
            Mode::CdPrompt => {
                let col = STATUS_TEXT_START as u16 + "cd: ".len() as u16 + cd_prompt_buf.len() as u16;
                execute!(stdout, cursor::MoveTo(col, rows - 1))?;
            }
            Mode::NewFilePrompt => {
                let prefix_len =
                    format!("New file in {}: ", current_dir.display()).len() as u16;
                let col = STATUS_TEXT_START as u16 + prefix_len + new_file_prompt_buf.len() as u16;
                execute!(stdout, cursor::MoveTo(col, rows - 1))?;
            }
            Mode::NewFolderPrompt => {
                let prefix_len =
                    format!("New folder in {}: ", current_dir.display()).len() as u16;
                let col = STATUS_TEXT_START as u16 + prefix_len + new_folder_prompt_buf.len() as u16;
                execute!(stdout, cursor::MoveTo(col, rows - 1))?;
            }
            _ => {
                execute!(stdout,
                    cursor::MoveTo(
                        gutter_w as u16 + cursor_x.saturating_sub(col_offset) as u16,
                        cursor_y.saturating_sub(row_offset) as u16,
                    )

                    )?;
            }
        }
        execute!(stdout, cursor::Show)?;

        stdout.flush()?;
        } // end else (normal buffer render)

        // ── Input ────────────────────────────────────────────────────────────
        if let Event::Key(KeyEvent { code, modifiers, kind, .. }) = event::read()? {
            if kind != KeyEventKind::Press {
                continue;
            }

            if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
                break 'editor;
            }

            if mode == Mode::StatusMsg {
                mode = Mode::Normal;
                status_msg.clear();
                continue;
            }

            match mode {
                // ── NORMAL ───────────────────────────────────────────────────
                Mode::Normal => match code {
                    KeyCode::Char('i') => mode = Mode::Insert,
                    KeyCode::Char('a') => {
                        if cursor_x < lines[cursor_y].len() {
                            cursor_x += 1;
                        }
                        mode = Mode::Insert;
                    }
                    KeyCode::Char('o') => {
                        cursor_y += 1;
                        lines.insert(cursor_y, Vec::new());
                        cursor_x = 0;
                        mode = Mode::Insert;
                    }
                    KeyCode::Char(':') => {
                        mode = Mode::Command;
                        command_buffer.clear();
                    }
                    KeyCode::Char('h') | KeyCode::Left => {
                        cursor_x = cursor_x.saturating_sub(1);
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        let len = lines[cursor_y].len();
                        if len > 0 && cursor_x + 1 < len {
                            cursor_x += 1;
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if cursor_y > 0 {
                            cursor_y -= 1;
                            cursor_x = cursor_x.min(lines[cursor_y].len().saturating_sub(1));
                        }
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if cursor_y + 1 < lines.len() {
                            cursor_y += 1;
                            cursor_x = cursor_x.min(lines[cursor_y].len().saturating_sub(1));
                        }
                    }
                    KeyCode::Char('x') => {
                        let line = &mut lines[cursor_y];
                        if cursor_x < line.len() {
                            line.remove(cursor_x);
                            if cursor_x > 0 && cursor_x >= line.len() {
                                cursor_x = line.len().saturating_sub(1);
                            }
                        }
                    }
                    KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let jump = (rows as usize) / 2;

                        cursor_y = (cursor_y + jump).min(lines.len().saturating_sub(1));
                    }
                    KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                        let jump = (rows as usize) / 2;

                        cursor_y = cursor_y.saturating_sub(jump);
                    }
                    KeyCode::PageDown => {
                        let jump = rows as usize - 2;

                        cursor_y = (cursor_y + jump).min(lines.len().saturating_sub(1));

                    }
                    KeyCode::PageUp => {
                        let jump = rows as usize - 2;

                        cursor_y = cursor_y.saturating_sub(jump);
                    }
                    KeyCode::Char('z') => {
                        row_offset = cursor_y.saturating_sub((rows as usize) / 2);
                        if row_offset > max_row_offset {
                            row_offset = max_row_offset;
                        }
                    }
                    _ => {}
                },

                // ── INSERT ───────────────────────────────────────────────────
                Mode::Insert => match code {
                    KeyCode::Esc => {
                        if autocomplete_active {
                            // First Esc just dismisses the popup.
                            autocomplete_active = false;
                        } else {
                            cursor_x = cursor_x.saturating_sub(1);
                            mode = Mode::Normal;
                        }
                    }
                    KeyCode::Enter => {
                        autocomplete_active = false;
                        let tail: Vec<char> = lines[cursor_y].drain(cursor_x..).collect();
                        cursor_y += 1;
                        lines.insert(cursor_y, tail);
                        cursor_x = 0;
                        dirty = true;
                    }
                    KeyCode::Backspace => {
                        if cursor_x > 0 {
                            let before = lines[cursor_y][cursor_x - 1];
                            let after = lines[cursor_y].get(cursor_x).copied();
                            cursor_x -= 1;
                            lines[cursor_y].remove(cursor_x);
                            // Backspacing the opener of an empty auto-inserted
                            // pair ("(|)") takes the closer with it too.
                            if let Some(after) = after {
                                if matching_closer(before) == Some(after) {
                                    lines[cursor_y].remove(cursor_x);
                                }
                            }
                            dirty = true;
                        } else if cursor_y > 0 {
                            let current = lines.remove(cursor_y);
                            cursor_y -= 1;
                            cursor_x = lines[cursor_y].len();
                            lines[cursor_y].extend(current);
                            dirty = true;
                        }
                        if autocomplete_active {
                            let (_, prefix) = word_prefix_at_cursor(&lines[cursor_y], cursor_x);
                            autocomplete_suggestions =
                                compute_suggestions(&prefix, &lines, &file_path);
                            autocomplete_selected = 0;
                            autocomplete_active = !autocomplete_suggestions.is_empty();
                        }
                    }
                    KeyCode::Left  => {
                        autocomplete_active = false;
                        cursor_x = cursor_x.saturating_sub(1);
                    }
                    KeyCode::Right => {
                        autocomplete_active = false;
                        if cursor_x < lines[cursor_y].len() { cursor_x += 1; }
                    }
                    KeyCode::Up => {
                        autocomplete_active = false;
                        if cursor_y > 0 {
                            cursor_y -= 1;
                            cursor_x = cursor_x.min(lines[cursor_y].len());
                        }
                    }
                    KeyCode::Down => {
                        autocomplete_active = false;
                        if cursor_y + 1 < lines.len() {
                            cursor_y += 1;
                            cursor_x = cursor_x.min(lines[cursor_y].len());
                        }
                    }
                    // Ctrl-N / Ctrl-P: open the popup (computing candidates for
                    // the word under the cursor) or, if it's already open,
                    // cycle forward/backward through the suggestions.
                    KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
                        if autocomplete_active && !autocomplete_suggestions.is_empty() {
                            autocomplete_selected =
                                (autocomplete_selected + 1) % autocomplete_suggestions.len();
                        } else {
                            let (_, prefix) = word_prefix_at_cursor(&lines[cursor_y], cursor_x);
                            autocomplete_suggestions =
                                compute_suggestions(&prefix, &lines, &file_path);
                            autocomplete_selected = 0;
                            autocomplete_active = !autocomplete_suggestions.is_empty();
                        }
                    }
                    KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => {
                        if autocomplete_active && !autocomplete_suggestions.is_empty() {
                            autocomplete_selected = if autocomplete_selected == 0 {
                                autocomplete_suggestions.len() - 1
                            } else {
                                autocomplete_selected - 1
                            };
                        } else {
                            let (_, prefix) = word_prefix_at_cursor(&lines[cursor_y], cursor_x);
                            autocomplete_suggestions =
                                compute_suggestions(&prefix, &lines, &file_path);
                            autocomplete_selected = 0;
                            autocomplete_active = !autocomplete_suggestions.is_empty();
                        }
                    }
                    // Tab: if the popup is open, accept the highlighted
                    // suggestion. Otherwise, advance to the next tab stop
                    // (a soft-tab of up to TAB_WIDTH spaces).
                    KeyCode::Tab => {
                        if autocomplete_active {
                            if let Some(choice) = autocomplete_suggestions.get(autocomplete_selected) {
                                accept_autocomplete(&mut lines[cursor_y], &mut cursor_x, choice);
                                dirty = true;
                            }
                            autocomplete_active = false;
                        } else {
                            let spaces = TAB_WIDTH - (cursor_x % TAB_WIDTH);
                            for _ in 0..spaces {
                                lines[cursor_y].insert(cursor_x, ' ');
                                cursor_x += 1;
                            }
                            dirty = true;
                        }
                    }
                    // Auto-close brackets/quotes (VSCode-style):
                    //   - typing an opener inserts the matching closer too,
                    //     leaving the cursor in between
                    //   - typing a quote that's already sitting right where
                    //     you are just types over it instead of nesting
                    //   - typing a closer that's already right there also
                    //     just steps over it
                    KeyCode::Char(c) => {
                        if let Some(closer) = matching_closer(c) {
                            let is_quote = c == '"' || c == '\'' || c == '`';
                            if is_quote && lines[cursor_y].get(cursor_x) == Some(&c) {
                                cursor_x += 1;
                            } else {
                                lines[cursor_y].insert(cursor_x, c);
                                lines[cursor_y].insert(cursor_x + 1, closer);
                                cursor_x += 1;
                            }
                            dirty = true;
                        } else if is_bracket_closer(c) && lines[cursor_y].get(cursor_x) == Some(&c) {
                            cursor_x += 1;
                        } else {
                            lines[cursor_y].insert(cursor_x, c);
                            cursor_x += 1;
                            dirty = true;
                        }
                        if autocomplete_active {
                            let (_, prefix) = word_prefix_at_cursor(&lines[cursor_y], cursor_x);
                            autocomplete_suggestions =
                                compute_suggestions(&prefix, &lines, &file_path);
                            autocomplete_selected = 0;
                            autocomplete_active = !autocomplete_suggestions.is_empty();
                        }
                    }
                    _ => {}
                },

                // ── COMMAND ──────────────────────────────────────────────────
                // Kept deliberately thin — only buffer input here.
                // All command logic lives in handle_command().
                Mode::Command => match code {
                    KeyCode::Esc => {
                        mode = Mode::Normal;
                        command_buffer.clear();
                    }
                    KeyCode::Backspace => { command_buffer.pop(); }
                    KeyCode::Char(c)   => { command_buffer.push(c); }
                    KeyCode::Enter => {
                        let cmd = command_buffer.trim().to_string();
                        command_buffer.clear();
                        let result = handle_command(
                            &cmd,
                            &mut mode,
                            &mut dirty,
                            &mut file_path,
                            &mut save_prompt_buf,
                            &mut new_file_prompt_buf,
                            &mut new_folder_prompt_buf,
                            &mut current_dir,
                            &mut browse_dir,
                            &mut browse_entries,
                            &mut browse_selected,
                            &mut browse_purpose,
                            &mut status_msg,
                            &mut lines,
                            &mut cursor_x,
                            &mut cursor_y,
                        );
                        if let CommandResult::Quit = result {
                            break 'editor;
                        }
                    }
                    _ => {}
                },

                // ── SAVE PROMPT ───────────────────────────────────────────────
                Mode::SavePrompt => match code {
                    KeyCode::Esc => {
                        status_msg = "Save cancelled.".to_string();
                        mode = Mode::StatusMsg;
                        save_prompt_buf.clear();
                    }
                    KeyCode::Backspace => { save_prompt_buf.pop(); }
                    KeyCode::Char(c)   => { save_prompt_buf.push(c); }
                    KeyCode::Enter => {
                        let name = save_prompt_buf.trim().to_string();
                        save_prompt_buf.clear();
                        if name.is_empty() {
                            status_msg = "Save cancelled (no filename given).".to_string();
                            mode = Mode::StatusMsg;
                        } else {
                            let path = PathBuf::from(&name);
                            match write_file(&path, &buffer_to_string(&lines)) {
                                Ok(_) => {
                                    dirty = false;
                                    file_path = Some(path.clone());
                                    status_msg = format!("Saved: {}", path.display());
                                }
                                Err(e) => {
                                    status_msg = format!("Error saving: {}", e);
                                }
                            }
                            mode = Mode::StatusMsg;
                        }
                    }
                    _ => {}
                },

                // ── OPEN PROMPT ───────────────────────────────────────────────
                Mode::OpenPrompt => match code {
                    KeyCode::Esc => {
                        status_msg = "Open cancelled.".to_string();
                        mode = Mode::StatusMsg;
                        open_prompt_buf.clear();
                    }
                    KeyCode::Backspace => { open_prompt_buf.pop(); }
                    KeyCode::Char(c)   => { open_prompt_buf.push(c); }
                    KeyCode::Enter => {
                        let name = open_prompt_buf.trim().to_string();
                        open_prompt_buf.clear();
                        if name.is_empty() {
                            status_msg = "Open cancelled (no path given).".to_string();
                            mode = Mode::StatusMsg;
                        } else {
                            let path = resolve_against(&current_dir, &name);
                            do_open(
                                &path,
                                &mut mode,
                                &mut dirty,
                                &mut file_path,
                                &mut lines,
                                &mut cursor_x,
                                &mut cursor_y,
                                &mut status_msg,
                            );
                        }
                    }
                    _ => {}
                },

                // ── CD PROMPT ────────────────────────────────────────────────
                Mode::CdPrompt => match code {
                    KeyCode::Esc => {
                        status_msg = "cd cancelled.".to_string();
                        mode = Mode::StatusMsg;
                        cd_prompt_buf.clear();
                    }
                    KeyCode::Backspace => { cd_prompt_buf.pop(); }
                    KeyCode::Char(c)   => { cd_prompt_buf.push(c); }
                    KeyCode::Enter => {
                        let target = cd_prompt_buf.trim().to_string();
                        cd_prompt_buf.clear();
                        if target.is_empty() {
                            status_msg = "cd cancelled (no path given).".to_string();
                            mode = Mode::StatusMsg;
                        } else {
                            do_cd(&mut current_dir, &target, &mut status_msg, &mut mode);
                        }
                    }
                    _ => {}
                },

                // ── DIRECTORY / FILE BROWSER (:cd or :op, no argument) ───────
                Mode::DirBrowse => match code {
                    KeyCode::Esc | KeyCode::Char('q') => {
                        status_msg = match browse_purpose {
                            BrowsePurpose::Cd => "cd cancelled.".to_string(),
                            BrowsePurpose::Open => "Open cancelled.".to_string(),
                        };
                        mode = Mode::StatusMsg;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if !browse_entries.is_empty() {
                            browse_selected = (browse_selected + 1).min(browse_entries.len() - 1);
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        browse_selected = browse_selected.saturating_sub(1);
                    }
                    // On a directory: descend into it (either purpose).
                    // On a file: open it, but only when browsing for :op —
                    // in :cd mode files just aren't actionable.
                    KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
                        if let Some((name, is_dir)) = browse_entries.get(browse_selected) {
                            if *is_dir {
                                browse_dir = browse_dir.join(name);
                                browse_entries = list_dir_entries(&browse_dir);
                                browse_selected = 0;
                            } else if browse_purpose == BrowsePurpose::Open {
                                let path = browse_dir.join(name);
                                do_open(
                                    &path,
                                    &mut mode,
                                    &mut dirty,
                                    &mut file_path,
                                    &mut lines,
                                    &mut cursor_x,
                                    &mut cursor_y,
                                    &mut status_msg,
                                );
                            }
                        }
                    }
                    // Go up to the parent directory.
                    KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace => {
                        if let Some(parent) = browse_dir.parent() {
                            browse_dir = parent.to_path_buf();
                            browse_entries = list_dir_entries(&browse_dir);
                            browse_selected = 0;
                        }
                    }
                    // Confirm: use whatever directory is currently being
                    // browsed. Only meaningful in :cd mode.
                    KeyCode::Char('.') => {
                        if browse_purpose == BrowsePurpose::Cd {
                            current_dir = browse_dir.clone();
                            status_msg = format!("cd: {}", current_dir.display());
                            mode = Mode::StatusMsg;
                        }
                    }
                    // Escape hatch: type an arbitrary path instead of browsing.
                    KeyCode::Char('/') => match browse_purpose {
                        BrowsePurpose::Cd => {
                            cd_prompt_buf = browse_dir.display().to_string();
                            mode = Mode::CdPrompt;
                        }
                        BrowsePurpose::Open => {
                            open_prompt_buf =
                                format!("{}{}", browse_dir.display(), std::path::MAIN_SEPARATOR);
                            mode = Mode::OpenPrompt;
                        }
                    },
                    _ => {}
                },

                // ── NEW FILE PROMPT (:e) ─────────────────────────────────────
                Mode::NewFilePrompt => match code {
                    KeyCode::Esc => {
                        status_msg = "New file cancelled.".to_string();
                        mode = Mode::StatusMsg;
                        new_file_prompt_buf.clear();
                    }
                    KeyCode::Backspace => { new_file_prompt_buf.pop(); }
                    KeyCode::Char(c)   => { new_file_prompt_buf.push(c); }
                    KeyCode::Enter => {
                        let name = new_file_prompt_buf.trim().to_string();
                        new_file_prompt_buf.clear();
                        if name.is_empty() {
                            status_msg = "New file cancelled (no name given).".to_string();
                            mode = Mode::StatusMsg;
                        } else {
                            do_new_file(
                                &current_dir,
                                &name,
                                &mut mode,
                                &mut dirty,
                                &mut file_path,
                                &mut lines,
                                &mut cursor_x,
                                &mut cursor_y,
                                &mut status_msg,
                            );
                        }
                    }
                    _ => {}
                },

                // ── NEW FOLDER PROMPT (:a) ───────────────────────────────────
                Mode::NewFolderPrompt => match code {
                    KeyCode::Esc => {
                        status_msg = "New folder cancelled.".to_string();
                        mode = Mode::StatusMsg;
                        new_folder_prompt_buf.clear();
                    }
                    KeyCode::Backspace => { new_folder_prompt_buf.pop(); }
                    KeyCode::Char(c)   => { new_folder_prompt_buf.push(c); }
                    KeyCode::Enter => {
                        let name = new_folder_prompt_buf.trim().to_string();
                        new_folder_prompt_buf.clear();
                        if name.is_empty() {
                            status_msg = "New folder cancelled (no name given).".to_string();
                            mode = Mode::StatusMsg;
                        } else {
                            do_new_folder(&mut current_dir, &name, &mut status_msg, &mut mode);
                        }
                    }
                    _ => {}
                },

                Mode::StatusMsg => unreachable!(),
            }
        }
    }

    Ok(())

    }
