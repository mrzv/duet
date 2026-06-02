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

#[derive(Debug)]
pub enum AllResolution {
    Proceed,
    Abort,
    Force,
}

pub fn resolve_sequential(actions: &mut Actions, _verbose: bool) -> Result<AllResolution> {
    use console::{Key, Term};
    let term = Term::stdout();
    if num_unresolved_conflicts(actions.iter()) > 0 {
        term.write_line("Resolve conflicts:")?;

        for a in actions {
            if let Action::Conflict(_, _) = &a {
                term.write_line(format!("{}", a).as_str())?;
                term.write_line(actions::details(a).as_str())?;

                loop {
                    term.write_line("left/l = update local, right/r = update remote, c = keep conflict, n/a = abort")?;
                    match term.read_key()? {
                        Key::ArrowLeft | Key::Char('l') => {
                            *a = resolve_action(&a, Resolution::Local);
                        }
                        Key::ArrowRight | Key::Char('r') => {
                            *a = resolve_action(&a, Resolution::Remote);
                        }
                        Key::Char('c') => {
                            // keep as is
                        }
                        Key::Char('a') => {
                            term.clear_last_lines(1)?;
                            return Ok(AllResolution::Abort);
                        }
                        _ => {
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

    use dialoguer::Confirm;
    if !Confirm::new()
        .with_prompt("Do you want to continue?")
        .interact()?
    {
        Ok(AllResolution::Abort)
    } else {
        Ok(AllResolution::Proceed)
    }
}

pub fn resolve_interactive(actions: &mut Actions, verbose: bool) -> Result<AllResolution> {
    use console::{Key, Term};
    use std::ops::Rem;
    let term = Term::stderr();

    let (height, _width) = term.size();

    let mut page = 0;

    assert!(!actions.is_empty());

    let mut actions: Vec<&mut Action> = actions
        .iter_mut()
        .filter(|a| verbose || !a.is_identical())
        .collect();

    let capacity = height as usize - 3;
    let pages = (actions.len() as f64 / capacity as f64).ceil() as usize;

    let mut sel = 0;
    let mut height = 0;
    let mut num_conflicts = num_unresolved_conflicts(actions.iter().map(|a| &**a));

    let resolution = loop {
        term.write_line(
            format!(
                "{}, n/a = abort, f = force{} [{}]",
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

        match term.read_key()? {
            Key::ArrowDown | Key::Char('j') => {
                loop {
                    sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
                    if verbose || !actions[sel].is_identical() {
                        break;
                    }
                }
            }
            Key::ArrowUp | Key::Char('k') => {
                loop {
                    sel =
                        ((sel as i64 - 1 + actions.len() as i64) % (actions.len() as i64)) as usize;
                    if verbose || !actions[sel].is_identical() {
                        break;
                    }
                }
            }
            Key::Tab => {
                loop {
                    sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
                    if actions[sel].is_conflict() {
                        break;
                    }
                }
            }
            Key::BackTab => {
                loop {
                    sel =
                        ((sel as i64 - 1 + actions.len() as i64) % (actions.len() as i64)) as usize;
                    if actions[sel].is_conflict() {
                        break;
                    }
                }
            }
            Key::ArrowLeft | Key::Char('l') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    *actions[sel] = resolve_action(&actions[sel], Resolution::Local);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::ArrowRight | Key::Char('r') => {
                if actions[sel].is_conflict() {
                    if actions[sel].is_unresolved_conflict() {
                        num_conflicts -= 1;
                    }
                    *actions[sel] = resolve_action(&actions[sel], Resolution::Remote);
                }
                sel = (sel as u64 + 1).rem(actions.len() as u64) as usize;
            }
            Key::Char('c') => {
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
            Key::PageUp => {
                if page == 0 {
                    page = pages - 1;
                } else {
                    page -= 1;
                }

                sel = page * capacity;
            }
            Key::PageDown => {
                if page == pages - 1 {
                    page = 0;
                } else {
                    page += 1;
                }

                sel = page * capacity;
            }

            Key::Char('y') | Key::Char('g') if num_conflicts == 0 => {
                break AllResolution::Proceed;
            }

            Key::Escape | Key::Char('a') | Key::Char('n') => {
                break AllResolution::Abort;
            }

            Key::Char('f') => {
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
