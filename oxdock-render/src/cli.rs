//! Renderer CLI surface: usage, version, and flag parsing.
//!
//! The binary honors `--help`/`-h` and `--version`/`-V` like the
//! `oxdock` CLI, and fails closed on unknown `--` flags instead of
//! silently rendering with defaults (a typo like `--confg` must
//! never mean "run anyway").

/// Parsed renderer flags. Paths stay strings here; the caller
/// converts to guarded paths after parsing.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// Workspace root override (`--root <path>`).
    pub root: Option<String>,
    /// Config filename override (`--config <path>`).
    pub config: Option<String>,
    /// Print the generated child script instead of running it.
    pub print_child: bool,
    /// Write a template `render.json` and stop.
    pub init: bool,
    /// Print usage and stop.
    pub help: bool,
    /// Print the version and stop.
    pub version: bool,
}

/// Human-readable renderer version, printed for `--version`/`-V`.
pub fn version() -> String {
    format!("oxdock-render {}", env!("CARGO_PKG_VERSION"))
}

/// Human-readable renderer usage, printed for `--help`/`-h` and on
/// unknown flags. The version line stamps the built package, so the
/// text can never drift from the release.
pub fn usage() -> String {
    let version = version();
    let description = env!("CARGO_PKG_DESCRIPTION");
    indoc::formatdoc! {"
        {version}: {description}

        Usage: oxdock-render [OPTIONS]

        Options:
          --root <path>     workspace root (default: discovered)
          --config <path>   pipeline config (default: render.json)
          --print-child     print the generated script, skip the run
          --init            write a template render.json, then stop
          --help, -h        print this help and exit
          --version, -V, -v print the version and exit
    "}
}

/// Parse renderer flags, skipping the program name in position zero.
/// Fail-closed: unknown flags, missing values, and positionals all
/// bail with usage instead of running with defaults. A lone `--`
/// separator ends option parsing: anything after it is a positional
/// and fails (there are no positionals). `--init` scaffolds and
/// stops, so combining it with `--config`/`--print-child` fails
/// instead of silently ignoring them.
pub fn parse_options(args: &[String]) -> anyhow::Result<Options> {
    let mut opts = Options::default();
    let mut rest = args.iter().skip(1).peekable();
    let mut seen_separator = false;
    while let Some(arg) = rest.next() {
        if seen_separator {
            anyhow::bail!("unexpected argument '{arg}'\n\n{}", usage())
        }
        match arg.as_str() {
            "--" => {
                seen_separator = true;
            }
            "--root" => match rest.next() {
                Some(value) if !value.is_empty() && !value.starts_with('-') => {
                    opts.root = Some(value.clone());
                }
                Some(value) => {
                    anyhow::bail!("missing value for '--root' (got '{value}')\n\n{}", usage())
                }
                None => {
                    anyhow::bail!("missing value for '--root'\n\n{}", usage())
                }
            },
            "--config" => match rest.next() {
                Some(value) if !value.is_empty() && !value.starts_with('-') => {
                    opts.config = Some(value.clone());
                }
                Some(value) => {
                    anyhow::bail!(
                        "missing value for '--config' (got '{value}')\n\n{}",
                        usage()
                    )
                }
                None => {
                    anyhow::bail!("missing value for '--config'\n\n{}", usage())
                }
            },
            "--print-child" => opts.print_child = true,
            "--init" => opts.init = true,
            "--help" | "-h" => opts.help = true,
            "--version" | "-V" | "-v" => opts.version = true,
            flag if flag.starts_with("--") => {
                anyhow::bail!("unknown flag '{flag}'\n\n{}", usage())
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                anyhow::bail!("unknown flag '{flag}'\n\n{}", usage())
            }
            _ => {
                anyhow::bail!("unexpected argument '{arg}'\n\n{}", usage())
            }
        }
    }
    if opts.init && (opts.print_child || opts.config.is_some()) {
        anyhow::bail!(
            "'--init' cannot be combined with '--config' or '--print-child'\n\n{}",
            usage()
        )
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(flags: &[&str]) -> Vec<String> {
        std::iter::once("oxdock-render".to_string())
            .chain(flags.iter().map(|flag| flag.to_string()))
            .collect()
    }

    #[test]
    fn version_names_binary_and_package_version() {
        let text = version();
        assert!(text.starts_with("oxdock-render "), "{text}");
        assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
    }

    #[test]
    fn usage_lists_every_flag() {
        let text = usage();
        assert!(text.contains("Usage: oxdock-render"), "{text}");
        for flag in [
            "--root",
            "--config",
            "--print-child",
            "--init",
            "--help",
            "-h",
            "--version",
            "-V",
            "-v",
        ] {
            assert!(text.contains(flag), "usage names {flag}: {text}");
        }
    }

    #[test]
    fn parses_known_flags() {
        let opts = parse_options(&args(&["--root", "/tmp/ws", "--config", "custom.json"]))
            .expect("known flags parse");
        assert_eq!(
            opts,
            Options {
                root: Some("/tmp/ws".to_string()),
                config: Some("custom.json".to_string()),
                print_child: false,
                init: false,
                help: false,
                version: false,
            }
        );
        let opts = parse_options(&args(&["--print-child"])).expect("print-child parses");
        assert!(opts.print_child);
        let opts = parse_options(&args(&["--init"])).expect("init parses");
        assert!(opts.init);
    }

    #[test]
    fn parses_short_help_and_version() {
        let opts = parse_options(&args(&["-h"])).expect("short help parses");
        assert!(opts.help);
        let opts = parse_options(&args(&["-V"])).expect("short version parses");
        assert!(opts.version);
        let opts = parse_options(&args(&["-v"])).expect("lowercase version parses");
        assert!(opts.version);
    }

    #[test]
    fn skips_separator_and_defaults_empty() {
        let opts = parse_options(&args(&["--"])).expect("separator parses");
        assert_eq!(opts, Options::default());
        let opts = parse_options(&args(&[])).expect("empty parses");
        assert_eq!(opts, Options::default());
    }

    #[test]
    fn unknown_long_flag_fails_with_usage() {
        let err = parse_options(&args(&["--confg", "x.json"])).expect_err("typo must fail");
        let rendered = format!("{err:#}");
        assert!(rendered.contains("unknown flag '--confg'"), "{rendered}");
        assert!(rendered.contains("Usage: oxdock-render"), "{rendered}");
    }

    #[test]
    fn unknown_short_flag_and_positional_fail() {
        let err = parse_options(&args(&["-x"])).expect_err("bad short must fail");
        assert!(format!("{err:#}").contains("unknown flag '-x'"));
        let err = parse_options(&args(&["render.json"])).expect_err("positional must fail");
        assert!(format!("{err:#}").contains("unexpected argument 'render.json'"));
    }

    #[test]
    fn dangling_value_flag_fails_closed() {
        // A trailing `--root` with no value fails instead of
        // silently falling back to discovery.
        let err = parse_options(&args(&["--root"])).expect_err("dangling must fail");
        assert!(
            format!("{err:#}").contains("missing value for '--root'"),
            "{err:#}"
        );
        let err = parse_options(&args(&["--config"])).expect_err("dangling config must fail");
        assert!(
            format!("{err:#}").contains("missing value for '--config'"),
            "{err:#}"
        );
    }

    #[test]
    fn flag_value_cannot_be_another_flag() {
        // `--root --print-child` used to swallow the flag as the path
        // and silently drop `--print-child`.
        let err = parse_options(&args(&["--root", "--print-child"]))
            .expect_err("flag-as-value must fail");
        assert!(
            format!("{err:#}").contains("missing value for '--root'"),
            "{err:#}"
        );
        let err = parse_options(&args(&["--config", "--root", "/tmp/ws"]))
            .expect_err("flag-as-value must fail");
        assert!(
            format!("{err:#}").contains("missing value for '--config'"),
            "{err:#}"
        );
    }

    #[test]
    fn separator_terminates_option_parsing() {
        // `-- --init` must not execute `--init`: everything after the
        // separator is positional and fails (there are no positionals).
        let err = parse_options(&args(&["--", "--init"])).expect_err("post-separator must fail");
        assert!(
            format!("{err:#}").contains("unexpected argument '--init'"),
            "{err:#}"
        );
    }

    #[test]
    fn init_rejects_config_and_print_child() {
        // `--init` scaffolds and stops; combining it with run flags
        // used to silently ignore them.
        let err = parse_options(&args(&["--init", "--print-child"]))
            .expect_err("init+print-child must fail");
        assert!(
            format!("{err:#}").contains("'--init' cannot be combined"),
            "{err:#}"
        );
        let err = parse_options(&args(&["--init", "--config", "custom.json"]))
            .expect_err("init+config must fail");
        assert!(
            format!("{err:#}").contains("'--init' cannot be combined"),
            "{err:#}"
        );
    }
}
