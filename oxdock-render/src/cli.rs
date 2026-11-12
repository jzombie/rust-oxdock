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
    format!(
        "{version}: {description}\n\
         \n\
         Usage: oxdock-render [OPTIONS]\n\
         \n\
         Options:\n\
         \x20 --root <path>     workspace root (default: discovered)\n\
         \x20 --config <path>   pipeline config (default: render.json)\n\
         \x20 --print-child     print the generated script, skip the run\n\
         \x20 --init            write a template render.json, then stop\n\
         \x20 --help, -h        print this help and exit\n\
         \x20 --version, -V, -v print the version and exit\n",
        version = version(),
        description = env!("CARGO_PKG_DESCRIPTION"),
    )
}

/// Parse renderer flags, skipping the program name in position zero.
/// A lone `--` separator is skipped. Anything else starting with
/// `--` (or a short cluster that is not exactly `-h`/`-V`) fails
/// with the usage text instead of running with defaults.
pub fn parse_options(args: &[String]) -> anyhow::Result<Options> {
    let mut opts = Options::default();
    let mut rest = args.iter().skip(1).peekable();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--" => {}
            "--root" => {
                opts.root = rest.next().cloned();
            }
            "--config" => {
                opts.config = rest.next().cloned();
            }
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
        let opts = parse_options(&args(&[
            "--root",
            "/tmp/ws",
            "--config",
            "custom.json",
            "--print-child",
            "--init",
        ]))
        .expect("known flags parse");
        assert_eq!(
            opts,
            Options {
                root: Some("/tmp/ws".to_string()),
                config: Some("custom.json".to_string()),
                print_child: true,
                init: true,
                help: false,
                version: false,
            }
        );
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
    fn dangling_value_flag_falls_back_to_default() {
        // A trailing `--root` with no value keeps the old lenient read:
        // no value means no override, so discovery applies.
        let opts = parse_options(&args(&["--root"])).expect("dangling parses");
        assert_eq!(opts.root, None);
    }
}
