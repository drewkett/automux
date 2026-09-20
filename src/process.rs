use std::collections::HashMap;

/// A snapshot of the local process table, used to find out what a pane is
/// really running.
///
/// Restored panes are launched through a `sh -lc` wrapper that replays
/// scrollback before exec'ing the real command, so tmux reports the wrapper
/// shell rather than the application. Walking the pane's descendants recovers
/// the truth without depending on `pane_current_command`.
pub struct Tree {
    children: HashMap<u32, Vec<u32>>,
    commands: HashMap<u32, String>,
}

impl Tree {
    pub fn capture() -> Option<Self> {
        let output = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,comm="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut commands = HashMap::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut fields = line.trim().splitn(3, char::is_whitespace);
            let Some(pid) = fields.next().and_then(|v| v.trim().parse::<u32>().ok()) else {
                continue;
            };
            let Some(ppid) = fields.next().and_then(|v| v.trim().parse::<u32>().ok()) else {
                continue;
            };
            let Some(command) = fields.next() else {
                continue;
            };
            let name = command.trim().rsplit('/').next().unwrap_or("").to_owned();
            children.entry(ppid).or_default().push(pid);
            commands.insert(pid, name);
        }
        (!commands.is_empty()).then_some(Self { children, commands })
    }

    /// The deepest non-shell descendant of `root`, if there is one.
    ///
    /// This is what a pane is really running: the pane process is a shell, and
    /// a restored pane adds a `sh -lc` wrapper on top, so the interesting
    /// command is always further down.
    pub fn deepest_command(&self, root: u32) -> Option<&str> {
        const SHELLS: [&str; 6] = ["sh", "bash", "zsh", "fish", "dash", "ksh"];

        let mut stack = vec![(root, 0usize)];
        let mut best: Option<(usize, &str)> = None;
        let mut seen = 0usize;
        while let Some((pid, depth)) = stack.pop() {
            seen += 1;
            if seen > self.commands.len() + 1 {
                break;
            }
            if let Some(command) = self.commands.get(&pid) {
                if !SHELLS.contains(&command.as_str())
                    && best.is_none_or(|(best_depth, _)| depth > best_depth)
                {
                    best = Some((depth, command.as_str()));
                }
            }
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(
                    kids.iter()
                        .copied()
                        .filter(|kid| *kid != pid)
                        .map(|kid| (kid, depth + 1)),
                );
            }
        }
        best.map(|(_, command)| command)
    }

    /// Whether `name` is the pane process itself or any of its descendants.
    pub fn runs(&self, root: u32, name: &str) -> bool {
        let mut stack = vec![root];
        let mut seen = 0usize;
        while let Some(pid) = stack.pop() {
            // Guard against a malformed table describing a cycle.
            seen += 1;
            if seen > self.commands.len() + 1 {
                return false;
            }
            if self.commands.get(&pid).is_some_and(|value| value == name) {
                return true;
            }
            if let Some(kids) = self.children.get(&pid) {
                stack.extend(kids.iter().copied().filter(|kid| *kid != pid));
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(rows: &[(u32, u32, &str)]) -> Tree {
        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut commands = HashMap::new();
        for (pid, ppid, comm) in rows {
            children.entry(*ppid).or_default().push(*pid);
            commands.insert(*pid, (*comm).to_owned());
        }
        Tree { children, commands }
    }

    #[test]
    fn finds_a_command_below_the_pane_shell() {
        // A restored pane: tmux reports `sh`, codex is two levels down.
        let tree = tree(&[(10, 1, "sh"), (11, 10, "bash"), (12, 11, "codex")]);
        assert!(tree.runs(10, "codex"));
        assert!(tree.runs(10, "sh"));
        assert!(!tree.runs(10, "claude"));
        assert!(!tree.runs(11, "sh"));
    }

    #[test]
    fn deepest_command_skips_shells() {
        let nested = tree(&[(10, 1, "sh"), (11, 10, "bash"), (12, 11, "codex")]);
        assert_eq!(nested.deepest_command(10), Some("codex"));

        // Claude Code reports its version as its process title.
        let claude = tree(&[(10, 1, "zsh"), (11, 10, "2.1.278")]);
        assert_eq!(claude.deepest_command(10), Some("2.1.278"));

        // An idle shell has nothing below it.
        let idle = tree(&[(10, 1, "zsh")]);
        assert_eq!(idle.deepest_command(10), None);
    }

    #[test]
    fn deepest_command_tolerates_a_cycle() {
        let cyclic = tree(&[(10, 11, "sh"), (11, 10, "bash")]);
        assert_eq!(cyclic.deepest_command(10), None);
    }

    #[test]
    fn tolerates_a_cycle() {
        let tree = tree(&[(10, 11, "sh"), (11, 10, "bash")]);
        assert!(!tree.runs(10, "codex"));
    }
}
