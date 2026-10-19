//! Command-line arguments of the `partition-manager` executable.

use std::path::PathBuf;

pub const USAGE: &str = "\
Partition Manager

Usage: partition-manager [OPTIONS]

Options:
  --disk-file <PATH>  Show the disk image file PATH as a disk instead of the
                      physical disks (repeatable; disks are numbered from 0)
  --dry-run           Start with \"dry run\" enabled: Apply only simulates
  -h, --help          Print this help and exit
";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub disk_files: Vec<PathBuf>,
    pub dry_run: bool,
    pub help: bool,
}

/// Parse the arguments after the program name.
pub fn parse<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut out = Args::default();
    let mut it = args.into_iter().map(Into::into);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => out.help = true,
            "--dry-run" => out.dry_run = true,
            "--disk-file" => {
                let p = it.next().ok_or("--disk-file needs a path")?;
                out.disk_files.push(nonempty_path(&p)?);
            }
            _ => match a.strip_prefix("--disk-file=") {
                Some(p) => out.disk_files.push(nonempty_path(p)?),
                None => return Err(format!("unexpected argument {a:?}")),
            },
        }
    }
    Ok(out)
}

fn nonempty_path(p: &str) -> Result<PathBuf, String> {
    if p.is_empty() {
        Err("--disk-file needs a non-empty path".into())
    } else {
        Ok(PathBuf::from(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags() {
        assert_eq!(parse(Vec::<String>::new()).unwrap(), Args::default());
        let a = parse(["--disk-file", "a.img", "--dry-run", "--disk-file=b.vhd"]).unwrap();
        assert_eq!(a.disk_files, vec![PathBuf::from("a.img"), PathBuf::from("b.vhd")]);
        assert!(a.dry_run);
        assert!(!a.help);
        assert!(parse(["-h"]).unwrap().help);
        assert!(parse(["--help"]).unwrap().help);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(["--disk-file"]).unwrap_err().contains("--disk-file"));
        assert!(parse(["--disk-file="]).is_err());
        assert!(parse(["--bogus"]).unwrap_err().contains("--bogus"));
        assert!(parse(["stray"]).is_err());
    }
}
