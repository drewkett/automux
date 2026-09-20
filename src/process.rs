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
    fn tolerates_a_cycle() {
        let tree = tree(&[(10, 11, "sh"), (11, 10, "bash")]);
        assert!(!tree.runs(10, "codex"));
    }
}
