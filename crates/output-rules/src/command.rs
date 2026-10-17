//! Finding the program a shell command line runs, only to choose a rule.
//! Nothing here runs or rewrites the command.

/// Wrappers that run a later word as the program, with their options that
/// take the next word as a value.
const WRAPPERS: [(&str, &[&str]); 6] = [
    (
        "env",
        &["-u", "--unset", "-C", "--chdir", "-S", "--split-string"],
    ),
    ("time", &["-o", "--output", "-f", "--format"]),
    ("command", &[]),
    ("exec", &["-a"]),
    ("nice", &["-n", "--adjustment"]),
    ("nohup", &[]),
];
/// Git options that take the next word as their value.
const GIT_VALUE_OPTIONS: [&str; 5] = ["-C", "-c", "--git-dir", "--work-tree", "--namespace"];

/// The words of the command whose output a command line produces: the last
/// command of a `;`/`&&`/`||` list, or the first of its pipeline, with
/// leading variable assignments and wrappers such as `env` removed and the
/// program given without its directory. Quotes are honoured; expansions are
/// not performed.
pub fn program(line: &str) -> Vec<String> {
    let mut commands: Vec<Vec<String>> = vec![vec![]];
    let mut word = String::new();
    let mut quoted = false;
    let (mut single, mut double, mut piped) = (false, false, false);
    let mut chars = line.chars().peekable();
    // End the current word, keeping it unless it follows a pipe.
    let finish =
        |word: &mut String, quoted: &mut bool, commands: &mut Vec<Vec<String>>, piped: bool| {
            if (!word.is_empty() || *quoted)
                && !piped
                && let Some(command) = commands.last_mut()
            {
                command.push(std::mem::take(word));
            }
            word.clear();
            *quoted = false;
        };
    while let Some(c) = chars.next() {
        match c {
            '\'' if !double => {
                single = !single;
                quoted = true;
            }
            '"' if !single => {
                double = !double;
                quoted = true;
            }
            '\\' if !single => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            _ if single || double => word.push(c),
            // `2>&1` and `&>` are redirections, not the background operator.
            '&' if word.ends_with(['>', '<']) || chars.peek() == Some(&'>') => word.push(c),
            '|' if chars.peek() != Some(&'|') => {
                finish(&mut word, &mut quoted, &mut commands, piped);
                piped = true;
            }
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                finish(&mut word, &mut quoted, &mut commands, piped);
                if matches!(c, '&' | '|') && chars.peek() == Some(&c) {
                    chars.next();
                }
                piped = false;
                if commands.last().is_some_and(|command| !command.is_empty()) {
                    commands.push(vec![]);
                }
            }
            c if c.is_whitespace() => finish(&mut word, &mut quoted, &mut commands, piped),
            c => word.push(c),
        }
    }
    finish(&mut word, &mut quoted, &mut commands, piped);
    let mut argv = commands
        .into_iter()
        .rev()
        .find(|command| !command.is_empty())
        .unwrap_or_default();
    let mut skip = 0;
    while let Some(word) = argv.get(skip) {
        if assignment(word) {
            skip += 1;
        } else if let Some((_, valued)) = WRAPPERS.iter().find(|(name, _)| name == word) {
            skip += 1;
            // The wrapper's options, up to `--` or the first other word.
            while let Some(option) = argv.get(skip).filter(|word| word.starts_with('-')) {
                skip += if option == "--" {
                    1
                } else if valued.contains(&option.as_str()) {
                    2
                } else {
                    1
                };
                if option == "--" {
                    break;
                }
            }
        } else {
            break;
        }
    }
    argv.drain(..skip.min(argv.len()));
    if let Some(first) = argv.first_mut()
        && let Some((_, name)) = first.rsplit_once('/')
        && !name.is_empty()
    {
        *first = name.to_owned();
    }
    argv
}

fn assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The subcommand of a `git` invocation, after any global options.
pub(crate) fn git_subcommand(argv: &[String]) -> Option<&str> {
    let mut words = argv.iter().map(String::as_str);
    if words.next()? != "git" {
        return None;
    }
    while let Some(word) = words.next() {
        if GIT_VALUE_OPTIONS.contains(&word) {
            words.next();
        } else if !word.starts_with('-') {
            return Some(word);
        }
    }
    None
}
