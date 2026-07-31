use color_eyre::eyre::Result;
use colored::*;

use crate::actions::{self, num_identical, num_unresolved_conflicts, Action, Actions};
use crate::scan::Change;

enum Resolution {
    Local,
    Remote,
}

fn resolve_action(action: &Action, resolution: Resolution) -> Action {
    match action {
        Action::Conflict(lc, rc)
        | Action::ResolvedLocal((lc, rc), _)
        | Action::ResolvedRemote((lc, rc), _) => match resolution {
            Resolution::Local => match (lc, rc) {
                (Change::Added(ln), Change::Added(rn)) => Action::ResolvedLocal(
                    (lc.clone(), rc.clone()),
                    Change::Modified(ln.clone(), rn.clone()),
                ),
                (Change::Removed(_), Change::Modified(_, rn)) => {
                    Action::ResolvedLocal((lc.clone(), rc.clone()), Change::Added(rn.clone()))
                }
                (Change::Modified(_lo, ln), Change::Modified(_ro, rn)) => Action::ResolvedLocal(
                    (lc.clone(), rc.clone()),
                    Change::Modified(ln.clone(), rn.clone()),
                ),
                (Change::Modified(_, ln), Change::Removed(_)) => {
                    Action::ResolvedLocal((lc.clone(), rc.clone()), Change::Removed(ln.clone()))
                }
                _ => unreachable!(),
            },
            Resolution::Remote => match (lc, rc) {
                (Change::Added(ln), Change::Added(rn)) => Action::ResolvedRemote(
                    (lc.clone(), rc.clone()),
                    Change::Modified(rn.clone(), ln.clone()),
                ),
                (Change::Modified(_, ln), Change::Removed(_rn)) => {
                    Action::ResolvedRemote((lc.clone(), rc.clone()), Change::Added(ln.clone()))
                }
                (Change::Modified(_lo, ln), Change::Modified(_ro, rn)) => Action::ResolvedRemote(
                    (lc.clone(), rc.clone()),
                    Change::Modified(rn.clone(), ln.clone()),
                ),
                (Change::Removed(_ln), Change::Modified(_, rn)) => {
                    Action::ResolvedRemote((lc.clone(), rc.clone()), Change::Removed(rn.clone()))
                }
                _ => unreachable!(),
            },
        },
        _ => action.clone(),
    }
}

pub fn show_actions(actions: &Actions, verbose: bool) {
    let num_identical = num_identical(actions.iter());
    for a in actions {
        if verbose || !a.is_identical() {
            println!("{}", a);
        }
    }
    if !verbose && num_identical > 0 {
        println!(
            "Skipped {} identical changes (use --verbose to show all)",
            num_identical
        );
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AllResolution {
    Proceed,
    Abort,
    Force,
    Interrupted,
}

#[derive(Debug, PartialEq, Eq)]
enum ConflictPromptChoice {
    Local,
    Remote,
    Keep,
    Abort,
    Interrupted,
}

fn conflict_prompt_choice(key: console::Key) -> Option<ConflictPromptChoice> {
    use console::Key;

    match key {
        Key::ArrowLeft | Key::Char('l') => Some(ConflictPromptChoice::Local),
        Key::ArrowRight | Key::Char('r') => Some(ConflictPromptChoice::Remote),
        Key::Char('c') => Some(ConflictPromptChoice::Keep),
        Key::Escape | Key::Char('a') | Key::Char('n') => Some(ConflictPromptChoice::Abort),
        Key::CtrlC => Some(ConflictPromptChoice::Interrupted),
        _ => None,
    }
}

fn confirmation_prompt_resolution(key: console::Key) -> Option<AllResolution> {
    use console::Key;

    match key {
        Key::Char('y') | Key::Char('Y') => Some(AllResolution::Proceed),
        Key::Escape | Key::Char('n') | Key::Char('N') => Some(AllResolution::Abort),
        Key::CtrlC => Some(AllResolution::Interrupted),
        _ => None,
    }
}

pub fn resolve_sequential(actions: &mut Actions, _verbose: bool) -> Result<AllResolution> {
    use console::Term;
    let term = Term::stdout();
    if num_unresolved_conflicts(actions.iter()) > 0 {
        term.write_line("Resolve conflicts:")?;

        for a in actions {
            if let Action::Conflict(_, _) = &a {
                term.write_line(format!("{}", a).as_str())?;
                term.write_line(actions::details(a).as_str())?;

                loop {
                    term.write_line("left/l = update local, right/r = update remote, c = keep conflict, n/a = abort")?;
                    match conflict_prompt_choice(term.read_key()?) {
                        Some(ConflictPromptChoice::Local) => {
                            *a = resolve_action(&a, Resolution::Local);
                        }
                        Some(ConflictPromptChoice::Remote) => {
                            *a = resolve_action(&a, Resolution::Remote);
                        }
                        Some(ConflictPromptChoice::Keep) => {
                            // keep as is
                        }
                        Some(ConflictPromptChoice::Abort) => {
                            term.clear_last_lines(1)?;
                            return Ok(AllResolution::Abort);
                        }
                        Some(ConflictPromptChoice::Interrupted) => {
                            return Ok(AllResolution::Interrupted);
                        }
                        None => {
                            term.clear_last_lines(1)?;
                            continue;
                        }
                    }
                    term.clear_last_lines(3)?;
                    term.write_line(format!("{}", a).as_str())?;
                    break;
                }
            }
        }
    }

    term.write_str("Do you want to continue? [y/n] ")?;
    term.flush()?;
    loop {
        if let Some(resolution) = confirmation_prompt_resolution(term.read_key()?) {
            if resolution == AllResolution::Interrupted {
                return Ok(resolution);
            }
            term.write_line("")?;
            return Ok(resolution);
        }
    }
}

pub fn resolve_interactive(actions: &mut Actions, verbose: bool) -> Result<AllResolution> {
    use console::Term;
    use std::ops::Rem;
    let term = Term::stderr();
    let _cursor_restore = CursorRestore(&term);

    let (height, _width) = term.size();

    let mut page = 0;

    assert!(!actions.is_empty());

    let mut actions: Vec<&mut Action> = actions
        .iter_mut()
        .filter(|a| verbose || !a.is_identical())
        .collect();

    let capacity = (height as usize).saturating_sub(3).max(1);

    let mut sel = 0;
    let mut height = 0;
    let mut num_conflicts = num_unresolved_conflicts(actions.iter().map(|a| &**a));

    let resolution = loop {
        term.write_line(
            format!(
                "{}, Shift+Up/Shift+Down = page, n/a = abort, f = force{} [{}]",
                if num_conflicts == 0 {
                    "y/g = proceed".bright_green()
                } else {
                    "Tab/S-Tab = next/previous conflict".bright_yellow()
                },
                if actions[sel].is_conflict() {
                    ", left/l = update local, right/r = update remote, c = keep conflict"
                } else {
                    ""
                },
                num_conflicts
            )
            .as_str(),
        )?;
        term.write_line(actions::details(&actions[sel]).as_str())?;
        height += 2;

        for (idx, action) in actions
            .iter()
            .enumerate()
            .skip(page * capacity)
            .take(capacity)
        {
            term.write_line(
                format!("{} {}", (if sel == idx { ">" } else { " " }).cyan(), action).as_str(),
            )?;
            height += 1;
        }

        term.hide_cursor()?;
        term.flush()?;

        let key = match read_interactive_key(&term) {
            Ok(key) => key,
            Err(error) => {
                term.show_cursor()?;
                term.flush()?;
                return Err(error);
            }
        };
        match key {
            InteractiveKey::ArrowDown | InteractiveKey::Char('j') => loop {
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
                if verbose || !actions[sel].is_identical() {
                    break;
                }
            },
            InteractiveKey::ArrowUp | InteractiveKey::Char('k') => loop {
                sel = ((sel as i64 - 1 + actions.len() as i64) % (actions.len() as i64)) as usize;
                if verbose || !actions[sel].is_identical() {
                    break;
                }
            },
            InteractiveKey::Tab => {
                if let Some(next) = next_conflict_index(&actions, sel, 1) {
                    sel = next;
                }
            }
            InteractiveKey::BackTab => {
                if let Some(previous) = next_conflict_index(&actions, sel, -1) {
                    sel = previous;
                }
            }
            InteractiveKey::ArrowLeft | InteractiveKey::Char('l') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    *actions[sel] = resolve_action(&actions[sel], Resolution::Local);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            InteractiveKey::ArrowRight | InteractiveKey::Char('r') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    *actions[sel] = resolve_action(&actions[sel], Resolution::Remote);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            InteractiveKey::Char('c') => {
                if actions[sel].is_conflict() {
                    if !actions[sel].is_unresolved_conflict() {
                        match &actions[sel] {
                            Action::ResolvedLocal((lc, rc), _)
                            | Action::ResolvedRemote((lc, rc), _) => {
                                *actions[sel] = Action::Conflict(lc.clone(), rc.clone());
                            }
                            _ => unreachable!(),
                        }
                        num_conflicts += 1;
                    }
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            InteractiveKey::ShiftUp => {
                sel = page_selection(sel, capacity, actions.len(), -1);
            }
            InteractiveKey::ShiftDown => {
                sel = page_selection(sel, capacity, actions.len(), 1);
            }

            InteractiveKey::Char('y') | InteractiveKey::Char('g') if num_conflicts == 0 => {
                break AllResolution::Proceed;
            }

            InteractiveKey::Escape | InteractiveKey::Char('a') | InteractiveKey::Char('n') => {
                break AllResolution::Abort;
            }

            InteractiveKey::CtrlC => {
                return Ok(AllResolution::Interrupted);
            }

            InteractiveKey::Char('f') => {
                break AllResolution::Force;
            }

            _ => {}
        }

        if sel < page * capacity || sel >= (page + 1) * capacity {
            page = sel / capacity;
        }

        term.clear_last_lines(height)?;
        height = 0;
    };

    term.clear_last_lines(height)?;
    term.show_cursor()?;
    term.flush()?;

    Ok(resolution)
}

struct CursorRestore<'a>(&'a console::Term);

impl Drop for CursorRestore<'_> {
    fn drop(&mut self) {
        let _ = self.0.show_cursor();
        let _ = self.0.flush();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InteractiveKey {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    ShiftUp,
    ShiftDown,
    Tab,
    BackTab,
    Escape,
    CtrlC,
    Char(char),
    Other,
}

fn read_interactive_key(term: &console::Term) -> Result<InteractiveKey> {
    if !term.is_term() {
        return Err(color_eyre::eyre::eyre!(
            "interactive conflict resolution requires a terminal"
        ));
    }
    read_unix_interactive_key().map_err(Into::into)
}

fn parse_interactive_escape_sequence(sequence: &[u8]) -> InteractiveKey {
    match sequence {
        [] => InteractiveKey::Escape,
        b"[A" | b"OA" => InteractiveKey::ArrowUp,
        b"[B" | b"OB" => InteractiveKey::ArrowDown,
        b"[C" | b"OC" => InteractiveKey::ArrowRight,
        b"[D" | b"OD" => InteractiveKey::ArrowLeft,
        b"[Z" => InteractiveKey::BackTab,
        b"[1;2A" => InteractiveKey::ShiftUp,
        b"[1;2B" => InteractiveKey::ShiftDown,
        _ => InteractiveKey::Other,
    }
}

fn interactive_escape_sequence_complete(sequence: &[u8]) -> bool {
    match sequence {
        [] => false,
        [b'O'] => false,
        [b'O', _] => true,
        [b'[', b'A' | b'B' | b'C' | b'D' | b'H' | b'F' | b'Z'] => true,
        [b'[', rest @ ..] => rest
            .last()
            .map(|byte| (b'@'..=b'~').contains(byte))
            .unwrap_or(false),
        _ => true,
    }
}

fn parse_interactive_byte(byte: u8) -> InteractiveKey {
    match byte {
        b'\x03' => InteractiveKey::CtrlC,
        b'\t' => InteractiveKey::Tab,
        b'\r' | b'\n' => InteractiveKey::Other,
        b if b.is_ascii() && !b.is_ascii_control() => InteractiveKey::Char(b as char),
        _ => InteractiveKey::Other,
    }
}

#[cfg(unix)]
fn read_unix_interactive_key() -> std::io::Result<InteractiveKey> {
    use std::os::fd::{AsRawFd, RawFd};

    const ESCAPE_SEQUENCE_TIMEOUT_MS: i32 = 20;
    const MAX_ESCAPE_SEQUENCE_BYTES: usize = 8;

    struct RawModeGuard {
        fd: RawFd,
        original: libc::termios,
    }

    fn tcsetattr_retry(
        fd: RawFd,
        optional_actions: libc::c_int,
        termios: &libc::termios,
    ) -> std::io::Result<()> {
        loop {
            let result = unsafe { libc::tcsetattr(fd, optional_actions, termios) };
            if result == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = tcsetattr_retry(self.fd, libc::TCSADRAIN, &self.original);
        }
    }

    enum TerminalInput {
        Stdin(std::io::Stdin),
        Tty(std::fs::File),
    }

    impl AsRawFd for TerminalInput {
        fn as_raw_fd(&self) -> RawFd {
            match self {
                TerminalInput::Stdin(stdin) => stdin.as_raw_fd(),
                TerminalInput::Tty(file) => file.as_raw_fd(),
            }
        }
    }

    fn terminal_input() -> std::io::Result<TerminalInput> {
        let stdin = std::io::stdin();
        if unsafe { libc::isatty(stdin.as_raw_fd()) != 0 } {
            Ok(TerminalInput::Stdin(stdin))
        } else {
            let tty = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")?;
            Ok(TerminalInput::Tty(tty))
        }
    }

    fn wait_for_input(fd: RawFd, timeout_ms: Option<i32>) -> std::io::Result<bool> {
        if fd < 0 || fd as usize >= libc::FD_SETSIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "terminal file descriptor is too large for select",
            ));
        }
        loop {
            let mut read_set: libc::fd_set = unsafe { std::mem::zeroed() };
            unsafe {
                libc::FD_ZERO(&mut read_set);
                libc::FD_SET(fd, &mut read_set);
            }
            let mut timeout_storage;
            let timeout_ptr = if let Some(timeout_ms) = timeout_ms {
                timeout_storage = libc::timeval {
                    tv_sec: (timeout_ms / 1000) as _,
                    tv_usec: ((timeout_ms % 1000) * 1000) as _,
                };
                &mut timeout_storage as *mut libc::timeval
            } else {
                std::ptr::null_mut()
            };
            let result = unsafe {
                libc::select(
                    fd + 1,
                    &mut read_set,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    timeout_ptr,
                )
            };
            if result < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            } else {
                return Ok(result > 0 && unsafe { libc::FD_ISSET(fd, &read_set) });
            }
        }
    }

    fn read_byte(fd: RawFd, timeout_ms: Option<i32>) -> std::io::Result<Option<u8>> {
        if !wait_for_input(fd, timeout_ms)? {
            return Ok(None);
        }
        let mut byte = 0u8;
        let read = loop {
            let read = unsafe { libc::read(fd, &mut byte as *mut u8 as *mut _, 1) };
            if read >= 0
                || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break read;
            }
        };
        if read < 0 {
            Err(std::io::Error::last_os_error())
        } else if read == 0 {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "reached end of terminal input",
            ))
        } else {
            Ok(Some(byte))
        }
    }

    let input = terminal_input()?;
    let fd = input.as_raw_fd();
    let mut termios = std::mem::MaybeUninit::uninit();
    if unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut raw = unsafe { termios.assume_init() };
    let original = raw;
    unsafe { libc::cfmakeraw(&mut raw) };
    raw.c_oflag = original.c_oflag;
    tcsetattr_retry(fd, libc::TCSADRAIN, &raw)?;
    let _guard = RawModeGuard { fd, original };

    let Some(first) = read_byte(fd, None)? else {
        return Ok(InteractiveKey::Other);
    };
    if first != b'\x1b' {
        return Ok(parse_interactive_byte(first));
    }

    let mut sequence = Vec::new();
    while sequence.len() < MAX_ESCAPE_SEQUENCE_BYTES {
        match read_byte(fd, Some(ESCAPE_SEQUENCE_TIMEOUT_MS))? {
            Some(byte) => {
                sequence.push(byte);
                if interactive_escape_sequence_complete(&sequence) {
                    break;
                }
            }
            None => break,
        }
    }
    Ok(parse_interactive_escape_sequence(&sequence))
}

#[cfg(not(unix))]
fn read_unix_interactive_key() -> std::io::Result<InteractiveKey> {
    Ok(InteractiveKey::Other)
}

fn page_selection(sel: usize, capacity: usize, len: usize, step: isize) -> usize {
    let pages = (len + capacity - 1) / capacity;
    let page = sel / capacity;
    let row = sel % capacity;
    let next_page = (page as isize + step).rem_euclid(pages as isize) as usize;

    (next_page * capacity + row).min(len - 1)
}

fn next_conflict_index(actions: &[&mut Action], sel: usize, step: isize) -> Option<usize> {
    if actions.is_empty() {
        return None;
    }

    let len = actions.len() as isize;
    let mut idx = sel as isize;
    for _ in 0..actions.len() {
        idx = (idx + step).rem_euclid(len);
        if actions[idx as usize].is_conflict() {
            return Some(idx as usize);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::DirEntryWithMeta;
    use std::path::PathBuf;

    fn entry(path: &str, checksum: u32) -> DirEntryWithMeta {
        DirEntryWithMeta::test_file(PathBuf::from(path), checksum)
    }

    #[test]
    fn local_resolution_turns_added_added_conflict_into_local_update() {
        let local = Change::Added(entry("file.txt", 1));
        let remote = Change::Added(entry("file.txt", 2));
        let resolved = resolve_action(
            &Action::Conflict(local.clone(), remote.clone()),
            Resolution::Local,
        );

        match resolved {
            Action::ResolvedLocal(
                (original_local, original_remote),
                Change::Modified(from, to),
            ) => {
                assert_eq!(original_local.path(), local.path());
                assert_eq!(original_remote.path(), remote.path());
                assert_eq!(from.checksum(), 1);
                assert_eq!(to.checksum(), 2);
            }
            _ => panic!("expected local resolution"),
        }
    }

    #[test]
    fn remote_resolution_turns_modified_removed_conflict_into_remote_add() {
        let old = entry("file.txt", 1);
        let local_new = entry("file.txt", 2);
        let remote_old = entry("file.txt", 1);
        let local = Change::Modified(old, local_new.clone());
        let remote = Change::Removed(remote_old);
        let resolved = resolve_action(
            &Action::Conflict(local.clone(), remote.clone()),
            Resolution::Remote,
        );

        match resolved {
            Action::ResolvedRemote((original_local, original_remote), Change::Added(added)) => {
                assert_eq!(original_local.path(), local.path());
                assert_eq!(original_remote.path(), remote.path());
                assert_eq!(added.checksum(), local_new.checksum());
            }
            _ => panic!("expected remote resolution"),
        }
    }

    #[test]
    fn next_conflict_index_returns_none_without_conflicts() {
        let mut first = Action::Remote(Change::Added(entry("first.txt", 1)));
        let mut second = Action::Remote(Change::Added(entry("second.txt", 2)));
        let actions = vec![&mut first, &mut second];

        assert_eq!(next_conflict_index(&actions, 0, 1), None);
        assert_eq!(next_conflict_index(&actions, 0, -1), None);
    }

    #[test]
    fn next_conflict_index_wraps_between_conflicts() {
        let mut first = Action::Remote(Change::Added(entry("first.txt", 1)));
        let mut second = Action::Conflict(
            Change::Added(entry("second.txt", 2)),
            Change::Added(entry("second.txt", 3)),
        );
        let mut third = Action::Remote(Change::Added(entry("third.txt", 4)));
        let actions = vec![&mut first, &mut second, &mut third];

        assert_eq!(next_conflict_index(&actions, 0, 1), Some(1));
        assert_eq!(next_conflict_index(&actions, 2, 1), Some(1));
        assert_eq!(next_conflict_index(&actions, 0, -1), Some(1));
    }

    #[test]
    fn page_selection_moves_by_a_page_and_preserves_the_row() {
        assert_eq!(page_selection(2, 5, 14, 1), 7);
        assert_eq!(page_selection(7, 5, 14, -1), 2);
    }

    #[test]
    fn page_selection_wraps_and_clamps_partial_pages() {
        assert_eq!(page_selection(2, 5, 14, -1), 12);
        assert_eq!(page_selection(12, 5, 14, 1), 2);
        assert_eq!(page_selection(9, 5, 12, 1), 11);
    }

    #[test]
    fn interactive_escape_parser_recognizes_shift_arrows() {
        assert_eq!(
            parse_interactive_escape_sequence(b"[1;2A"),
            InteractiveKey::ShiftUp
        );
        assert_eq!(
            parse_interactive_escape_sequence(b"[1;2B"),
            InteractiveKey::ShiftDown
        );
    }

    #[test]
    fn interactive_escape_parser_ignores_page_keys() {
        assert_eq!(
            parse_interactive_escape_sequence(b"[5~"),
            InteractiveKey::Other
        );
        assert_eq!(
            parse_interactive_escape_sequence(b"[6~"),
            InteractiveKey::Other
        );
    }

    #[test]
    fn interactive_byte_parser_recognizes_ctrl_c() {
        assert_eq!(parse_interactive_byte(b'\x03'), InteractiveKey::CtrlC);
    }

    #[test]
    fn sequential_conflict_prompt_classifies_ctrl_c_as_interrupted() {
        assert_eq!(
            conflict_prompt_choice(console::Key::CtrlC),
            Some(ConflictPromptChoice::Interrupted)
        );
        assert_eq!(
            conflict_prompt_choice(console::Key::Escape),
            Some(ConflictPromptChoice::Abort)
        );
        assert_eq!(
            conflict_prompt_choice(console::Key::Char('n')),
            Some(ConflictPromptChoice::Abort)
        );
    }

    #[test]
    fn sequential_confirmation_classifies_ctrl_c_separately_from_no() {
        assert_eq!(
            confirmation_prompt_resolution(console::Key::CtrlC),
            Some(AllResolution::Interrupted)
        );
        assert_eq!(
            confirmation_prompt_resolution(console::Key::Char('n')),
            Some(AllResolution::Abort)
        );
        assert_eq!(
            confirmation_prompt_resolution(console::Key::Char('y')),
            Some(AllResolution::Proceed)
        );
    }

    #[test]
    fn interactive_escape_parser_keeps_existing_navigation_keys() {
        assert_eq!(
            parse_interactive_escape_sequence(b"[A"),
            InteractiveKey::ArrowUp
        );
        assert_eq!(
            parse_interactive_escape_sequence(b"[B"),
            InteractiveKey::ArrowDown
        );
        assert_eq!(
            parse_interactive_escape_sequence(b"[C"),
            InteractiveKey::ArrowRight
        );
        assert_eq!(
            parse_interactive_escape_sequence(b"[D"),
            InteractiveKey::ArrowLeft
        );
        assert_eq!(
            parse_interactive_escape_sequence(b"[Z"),
            InteractiveKey::BackTab
        );
    }

    #[test]
    fn interactive_escape_completion_stops_after_one_key_sequence() {
        assert!(!interactive_escape_sequence_complete(b"["));
        assert!(!interactive_escape_sequence_complete(b"O"));
        assert!(!interactive_escape_sequence_complete(b"[1"));
        assert!(!interactive_escape_sequence_complete(b"[1;2"));
        assert!(interactive_escape_sequence_complete(b"[B"));
        assert!(interactive_escape_sequence_complete(b"[1;2A"));
        assert!(interactive_escape_sequence_complete(b"[5~"));
        assert!(!interactive_escape_sequence_complete(b"[B\x1b"));
    }

    #[test]
    fn interactive_key_reader_rejects_non_terminal_output() {
        let err = read_interactive_key(&console::Term::buffered_stderr()).unwrap_err();
        assert!(err.to_string().contains("requires a terminal"));
    }
}
