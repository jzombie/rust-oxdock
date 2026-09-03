use crate::ast::Arg;
use crate::command::{CommandMeta, FlagValueType};
use anyhow::{Result, anyhow};

/// Result of flag stripping: (extracted_flags, remaining_positional_args).
pub type StrippedArgs = (Vec<(String, Arg)>, Vec<Arg>);

/// Strip flags from raw arguments, returning (flags, positional_args).
///
/// Handles:
/// - POSIX `--` terminator (stops flag scanning)
/// - `--flag val` and `--flag=val` forms
/// - Unknown flags are rejected when the command has registered flags
/// - Unrecognized `--` on flag-less commands falls through as positional
pub fn strip_flags(args: Vec<Arg>, meta: &CommandMeta) -> Result<StrippedArgs> {
    if meta.flags.is_empty() {
        return Ok((Vec::new(), args));
    }

    let mut flags = Vec::new();
    let mut positional = Vec::new();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match &arg {
            // Quoted arguments are always positional, even if they start with `--`
            Arg::String(_, true) => {
                positional.push(arg);
            }
            Arg::String(s, false) if s == "--" => {
                positional.extend(iter);
                break;
            }
            Arg::String(s, false) if s.starts_with("--") => {
                let matched = meta.flags.iter().find(|f| {
                    s == f.long
                        || s.starts_with(f.long) && s.as_bytes().get(f.long.len()) == Some(&b'=')
                });
                match matched {
                    Some(flag_meta) => {
                        let value = if let Some(eq_pos) = s.find('=') {
                            Arg::String(s[eq_pos + 1..].to_string(), false)
                        } else if matches!(flag_meta.value_type, FlagValueType::Flag) {
                            Arg::String("true".into(), false)
                        } else {
                            iter.next()
                                .ok_or_else(|| anyhow!("{} requires a value", flag_meta.long))?
                        };
                        flags.push((flag_meta.name.to_string(), value));
                    }
                    None => {
                        return Err(anyhow!("unknown flag {} for command {}", s, meta.name));
                    }
                }
            }
            _ => positional.push(arg),
        }
    }
    Ok((flags, positional))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::FlagValueType;

    fn test_meta(flags: &'static [crate::command::FlagSpec]) -> CommandMeta {
        CommandMeta {
            name: "TEST_CMD",
            syntax: "TEST_CMD",
            summary: "test",
            description: "test",
            args: &[],
            flags,
            default_output: None,
            examples: &[],
        }
    }

    #[test]
    fn no_flags_passes_through() {
        let meta = test_meta(&[]);
        let args = vec![
            Arg::String("hello".into(), false),
            Arg::String("world".into(), false),
        ];
        let (flags, pos) = strip_flags(args, &meta).unwrap();
        assert!(flags.is_empty());
        assert_eq!(pos.len(), 2);
    }

    #[test]
    fn posix_terminator_stops_scanning() {
        let meta = test_meta(&[crate::command::FlagSpec {
            name: "hash",
            long: "--hash",
            value_type: FlagValueType::String,
            required: false,
            description: "",
        }]);
        let args = vec![
            Arg::String("--".into(), false),
            Arg::String("--hash".into(), false),
            Arg::String("abc123".into(), false),
        ];
        let (flags, pos) = strip_flags(args, &meta).unwrap();
        assert!(flags.is_empty());
        assert_eq!(pos.len(), 2);
        assert_eq!(pos[0].as_str(), "--hash");
        assert_eq!(pos[1].as_str(), "abc123");
    }

    #[test]
    fn flag_with_separate_value() {
        let meta = test_meta(&[crate::command::FlagSpec {
            name: "hash",
            long: "--hash",
            value_type: FlagValueType::String,
            required: false,
            description: "",
        }]);
        let args = vec![
            Arg::String("--hash".into(), false),
            Arg::String("abc123".into(), false),
            Arg::String("path.txt".into(), false),
        ];
        let (flags, pos) = strip_flags(args, &meta).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, "hash");
        assert_eq!(flags[0].1.as_str(), "abc123");
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0].as_str(), "path.txt");
    }

    #[test]
    fn flag_with_attached_value() {
        let meta = test_meta(&[crate::command::FlagSpec {
            name: "hash",
            long: "--hash",
            value_type: FlagValueType::String,
            required: false,
            description: "",
        }]);
        let args = vec![
            Arg::String("--hash=abc123".into(), false),
            Arg::String("path.txt".into(), false),
        ];
        let (flags, pos) = strip_flags(args, &meta).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, "hash");
        assert_eq!(flags[0].1.as_str(), "abc123");
        assert_eq!(pos.len(), 1);
    }

    #[test]
    fn boolean_flag() {
        let meta = test_meta(&[crate::command::FlagSpec {
            name: "dirty",
            long: "--include-dirty",
            value_type: FlagValueType::Flag,
            required: false,
            description: "",
        }]);
        let args = vec![
            Arg::String("--include-dirty".into(), false),
            Arg::String("rev".into(), false),
        ];
        let (flags, pos) = strip_flags(args, &meta).unwrap();
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].0, "dirty");
        assert_eq!(flags[0].1.as_str(), "true");
        assert_eq!(pos.len(), 1);
    }

    #[test]
    fn unknown_flag_rejected() {
        let meta = test_meta(&[crate::command::FlagSpec {
            name: "hash",
            long: "--hash",
            value_type: FlagValueType::String,
            required: false,
            description: "",
        }]);
        let args = vec![
            Arg::String("--hsh".into(), false),
            Arg::String("val".into(), false),
        ];
        let result = strip_flags(args, &meta);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown flag"));
    }

    #[test]
    fn flag_missing_value_is_error() {
        let meta = test_meta(&[crate::command::FlagSpec {
            name: "hash",
            long: "--hash",
            value_type: FlagValueType::String,
            required: false,
            description: "",
        }]);
        let args = vec![Arg::String("--hash".into(), false)];
        let result = strip_flags(args, &meta);
        assert!(result.is_err());
    }
}
