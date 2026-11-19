//! Finds the external dump tools and checks them against the server. Tools are
//! detected, never bundled: see
//! `docs/adr/202610092308-detect-installed-dump-tools-instead-of-bundling-them.md`.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use savoia_core::{Engine, ServerInfo};

/// An external client tool Savoia Studio can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    PgDump,
    PgRestore,
    Psql,
    Mysqldump,
    Mysql,
}

impl Tool {
    pub const ALL: [Tool; 5] = [
        Tool::PgDump,
        Tool::PgRestore,
        Tool::Psql,
        Tool::Mysqldump,
        Tool::Mysql,
    ];

    pub fn engine(self) -> Engine {
        match self {
            Tool::PgDump | Tool::PgRestore | Tool::Psql => Engine::Postgres,
            Tool::Mysqldump | Tool::Mysql => Engine::Mysql,
        }
    }

    pub fn for_engine(engine: Engine) -> &'static [Tool] {
        match engine {
            Engine::Postgres => &[Tool::PgDump, Tool::PgRestore, Tool::Psql],
            Engine::Mysql => &[Tool::Mysqldump, Tool::Mysql],
        }
    }

    pub fn name(self) -> &'static str {
        self.programs()[0]
    }

    /// Executable names to look for, preferred first. MariaDB installs its
    /// clients under their own names, sometimes without the MySQL aliases.
    fn programs(self) -> &'static [&'static str] {
        match self {
            Tool::PgDump => &["pg_dump"],
            Tool::PgRestore => &["pg_restore"],
            Tool::Psql => &["psql"],
            Tool::Mysqldump => &["mysqldump", "mariadb-dump"],
            Tool::Mysql => &["mysql", "mariadb"],
        }
    }
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which product a tool or server version belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Postgres,
    Mysql,
    Mariadb,
}

impl Flavor {
    fn engine(self) -> Engine {
        match self {
            Flavor::Postgres => Engine::Postgres,
            Flavor::Mysql | Flavor::Mariadb => Engine::Mysql,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Flavor::Postgres => "PostgreSQL",
            Flavor::Mysql => "MySQL",
            Flavor::Mariadb => "MariaDB",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Parses the leading `major[.minor[.patch]]` of `text`, ignoring whatever
    /// follows (`17beta1`, `16.4 (Debian …)`, `11.4.2-MariaDB`).
    fn parse_prefix(text: &str) -> Option<Version> {
        let end = text
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(text.len());
        let mut parts = text[..end].split('.').map(|part| part.parse::<u32>().ok());
        let major = parts.next()??;
        Some(Version {
            major,
            minor: parts.next().flatten().unwrap_or(0),
            patch: parts.next().flatten().unwrap_or(0),
        })
    }

    /// The numbers that name a Postgres release: the major alone from 10 on,
    /// major and minor before that (9.6).
    fn pg_release(self) -> (u32, u32) {
        if self.major >= 10 {
            (self.major, 0)
        } else {
            (self.major, self.minor)
        }
    }

    fn pg_release_label(self) -> String {
        match self.pg_release() {
            (major, 0) if major >= 10 => major.to_string(),
            (major, minor) => format!("{major}.{minor}"),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolVersion {
    pub flavor: Flavor,
    pub version: Version,
}

impl fmt::Display for ToolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.flavor.label(), self.version)
    }
}

/// Parses a tool's `--version` output, e.g. `pg_dump (PostgreSQL) 17.2`,
/// `mysqldump  Ver 8.4.3 for macos14 on arm64`,
/// `mysqldump  Ver 10.13 Distrib 5.7.44, for Linux` or
/// `mariadb-dump from 11.4.2-MariaDB, client 10.19`.
pub fn parse_tool_version(output: &str) -> Option<ToolVersion> {
    if let Some((_, rest)) = output.split_once("(PostgreSQL)") {
        return Some(ToolVersion {
            flavor: Flavor::Postgres,
            version: Version::parse_prefix(rest.trim_start())?,
        });
    }
    let flavor = if output.contains("MariaDB") {
        Flavor::Mariadb
    } else {
        Flavor::Mysql
    };
    // "Distrib" names the server release when "Ver" is only the client protocol.
    let version = ["Distrib ", "from ", "Ver "]
        .iter()
        .find_map(|marker| output.split_once(marker))
        .and_then(|(_, rest)| Version::parse_prefix(rest.trim_start()))?;
    Some(ToolVersion { flavor, version })
}

/// Reads the version from [`ServerInfo::version`] ("PostgreSQL 17.2",
/// "MySQL 8.4.3", "MariaDB 11.4.2").
pub fn parse_server_version(server: &ServerInfo) -> Option<ToolVersion> {
    let (product, rest) = server.version.split_once(' ')?;
    let flavor = match product {
        "PostgreSQL" => Flavor::Postgres,
        "MySQL" => Flavor::Mysql,
        "MariaDB" => Flavor::Mariadb,
        _ => return None,
    };
    Some(ToolVersion {
        flavor,
        version: Version::parse_prefix(rest.trim_start())?,
    })
}

/// Whether a tool can work with a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    Compatible,
    /// It will probably work, but the user should know why it might not.
    Warning(String),
    /// It will fail: use another binary or the built-in engine.
    Incompatible(String),
}

/// Checks `tool` at version `found` against `server`. `pg_dump` refuses to dump
/// a newer server, so that is the one hard failure; other mismatches warn.
pub fn check(tool: Tool, found: ToolVersion, server: ToolVersion) -> Compatibility {
    if found.flavor.engine() != tool.engine() || server.flavor.engine() != tool.engine() {
        return Compatibility::Incompatible(format!(
            "{tool} ({found}) cannot be used with {server}."
        ));
    }
    match tool.engine() {
        Engine::Postgres => check_postgres(tool, found.version, server.version),
        Engine::Mysql => check_mysql(tool, found, server),
    }
}

fn check_postgres(tool: Tool, found: Version, server: Version) -> Compatibility {
    if found.pg_release() >= server.pg_release() {
        return Compatibility::Compatible;
    }
    let wanted = server.pg_release_label();
    match tool {
        Tool::PgDump => Compatibility::Incompatible(format!(
            "pg_dump {found} is older than the server (PostgreSQL {server}) and will refuse \
             to dump it. Install pg_dump {wanted} or newer."
        )),
        _ => Compatibility::Warning(format!(
            "{tool} {found} is older than the server (PostgreSQL {server}); objects that need \
             a newer client may fail. Version {wanted} or newer is recommended."
        )),
    }
}

fn check_mysql(tool: Tool, found: ToolVersion, server: ToolVersion) -> Compatibility {
    if found.flavor != server.flavor {
        return Compatibility::Warning(format!(
            "{tool} comes from {found}, but the server is {server}; some objects or options \
             may not be supported."
        ));
    }
    let release = |v: Version| (v.major, v.minor);
    if release(found.version) < release(server.version) {
        return Compatibility::Warning(format!(
            "{tool} {} is older than the server ({server}); newer features may not dump or \
             restore correctly.",
            found.version
        ));
    }
    Compatibility::Compatible
}

/// A tool found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundTool {
    pub tool: Tool,
    pub path: PathBuf,
    pub version: ToolVersion,
}

/// Where to look for tools.
#[derive(Debug, Clone, Default)]
pub struct ToolSearch {
    /// A directory the user chose. A tool found here wins over newer ones elsewhere.
    pub custom_dir: Option<PathBuf>,
    /// Searched in order; among the copies found the newest wins.
    pub dirs: Vec<PathBuf>,
}

impl ToolSearch {
    /// `custom_dir`, then `PATH`, then the usual install directories for this
    /// OS. Those matter: Homebrew's `libpq` is keg-only, and apps launched
    /// from Finder get a minimal `PATH`.
    pub fn system(custom_dir: Option<PathBuf>) -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        dirs.extend(known_dirs());
        Self { custom_dir, dirs }
    }
}

/// Finds `tool`: the copy in the custom directory if there is one, otherwise
/// the newest copy anywhere else. Runs each candidate's `--version`, so call it
/// off the UI thread.
pub fn detect(tool: Tool, search: &ToolSearch) -> Option<FoundTool> {
    if let Some(dir) = &search.custom_dir
        && let Some(found) = executables(tool, dir).find_map(|path| probe(tool, path))
    {
        return Some(found);
    }
    let mut seen = HashSet::new();
    search
        .dirs
        .iter()
        .flat_map(|dir| executables(tool, dir))
        .filter(|path| seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())))
        .filter_map(|path| probe(tool, path))
        // `reduce` keeps the earliest of equal versions, so PATH order breaks ties.
        .reduce(|best, next| {
            if next.version.version > best.version.version {
                next
            } else {
                best
            }
        })
}

/// The files in `dir` that could be `tool`.
fn executables(tool: Tool, dir: &Path) -> impl Iterator<Item = PathBuf> {
    tool.programs()
        .iter()
        .map(move |program| dir.join(format!("{program}{}", std::env::consts::EXE_SUFFIX)))
        .filter(|path| path.is_file())
}

/// Runs `path --version` and keeps it only if it really is `tool`'s product.
fn probe(tool: Tool, path: PathBuf) -> Option<FoundTool> {
    let mut command = Command::new(&path);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = run_retrying_busy(&mut command)?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = parse_tool_version(&text)?;
    (version.flavor.engine() == tool.engine()).then_some(FoundTool {
        tool,
        path,
        version,
    })
}

/// Runs `command`, retrying briefly while the executable is "text file busy".
/// On Linux that happens right after the file was written (a package manager
/// is still installing it, or a process forked elsewhere still holds the
/// write descriptor), and clears up on its own.
fn run_retrying_busy(command: &mut Command) -> Option<std::process::Output> {
    for attempt in 0..5 {
        match command.output() {
            Ok(output) => return Some(output),
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 4 => {
                std::thread::sleep(std::time::Duration::from_millis(20 << attempt));
            }
            Err(_) => return None,
        }
    }
    None
}

/// Where the official installers and package managers put the clients.
fn known_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        for prefix in ["/opt/homebrew/opt", "/usr/local/opt"] {
            let prefix = Path::new(prefix);
            dirs.push(prefix.join("libpq/bin"));
            dirs.push(prefix.join("mysql-client/bin"));
            for versioned in [
                "libpq@",
                "postgresql@",
                "mysql-client@",
                "mysql@",
                "mariadb@",
            ] {
                dirs.extend(subdirs(prefix, versioned, "bin"));
            }
            dirs.push(prefix.join("mysql/bin"));
            dirs.push(prefix.join("mariadb/bin"));
        }
        dirs.extend(subdirs(
            Path::new("/Applications/Postgres.app/Contents/Versions"),
            "",
            "bin",
        ));
        dirs.push(PathBuf::from("/usr/local/mysql/bin"));
    } else if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(var).map(PathBuf::from) {
                dirs.extend(subdirs(&base.join("PostgreSQL"), "", "bin"));
                dirs.extend(subdirs(&base.join("MySQL"), "MySQL Server ", "bin"));
                dirs.extend(subdirs(&base, "MariaDB ", "bin"));
            }
        }
    } else {
        dirs.extend(subdirs(Path::new("/usr/lib/postgresql"), "", "bin"));
        dirs.extend(subdirs(Path::new("/usr"), "pgsql-", "bin"));
    }
    dirs
}

/// `parent/<name>/suffix` for each subdirectory whose name starts with
/// `prefix`, newest-looking last so the order is stable.
fn subdirs(parent: &Path, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(prefix))
        .map(|entry| entry.path().join(suffix))
        .filter(|dir| dir.is_dir())
        .collect();
    dirs.sort();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    fn tv(flavor: Flavor, major: u32, minor: u32, patch: u32) -> ToolVersion {
        ToolVersion {
            flavor,
            version: v(major, minor, patch),
        }
    }

    #[test]
    fn parses_postgres_tool_versions() {
        let cases = [
            ("pg_dump (PostgreSQL) 17.2\n", v(17, 2, 0)),
            ("pg_restore (PostgreSQL) 18.4", v(18, 4, 0)),
            (
                "psql (PostgreSQL) 16.4 (Ubuntu 16.4-1.pgdg22.04+1)",
                v(16, 4, 0),
            ),
            ("pg_dump (PostgreSQL) 9.6.24", v(9, 6, 24)),
            ("pg_dump (PostgreSQL) 18beta1", v(18, 0, 0)),
        ];
        for (output, expected) in cases {
            assert_eq!(
                parse_tool_version(output),
                Some(ToolVersion {
                    flavor: Flavor::Postgres,
                    version: expected
                }),
                "{output}"
            );
        }
    }

    #[test]
    fn parses_mysql_and_mariadb_tool_versions() {
        let cases = [
            (
                "mysqldump  Ver 8.4.3 for macos14 on arm64 (Homebrew)",
                tv(Flavor::Mysql, 8, 4, 3),
            ),
            (
                "mysql  Ver 26.7.0 for macos27.0 on arm64 (Homebrew)",
                tv(Flavor::Mysql, 26, 7, 0),
            ),
            (
                "mysqldump  Ver 10.13 Distrib 5.7.44, for Linux (x86_64)",
                tv(Flavor::Mysql, 5, 7, 44),
            ),
            (
                "mysql  Ver 15.1 Distrib 10.6.12-MariaDB, for debian-linux-gnu",
                tv(Flavor::Mariadb, 10, 6, 12),
            ),
            (
                "mariadb-dump from 11.4.2-MariaDB, client 10.19 for Linux (x86_64)",
                tv(Flavor::Mariadb, 11, 4, 2),
            ),
        ];
        for (output, expected) in cases {
            assert_eq!(parse_tool_version(output), Some(expected), "{output}");
        }
    }

    #[test]
    fn rejects_output_without_a_version() {
        assert_eq!(parse_tool_version(""), None);
        assert_eq!(parse_tool_version("command not found"), None);
        assert_eq!(parse_tool_version("pg_dump (PostgreSQL) devel"), None);
    }

    #[test]
    fn parses_server_versions() {
        let server = |version: &str| ServerInfo {
            version: version.into(),
        };
        assert_eq!(
            parse_server_version(&server("PostgreSQL 17.2")),
            Some(tv(Flavor::Postgres, 17, 2, 0))
        );
        assert_eq!(
            parse_server_version(&server("PostgreSQL 16.4 (Debian 16.4-1.pgdg120+1)")),
            Some(tv(Flavor::Postgres, 16, 4, 0))
        );
        assert_eq!(
            parse_server_version(&server("MySQL 8.4.3")),
            Some(tv(Flavor::Mysql, 8, 4, 3))
        );
        assert_eq!(
            parse_server_version(&server("MariaDB 11.4.2")),
            Some(tv(Flavor::Mariadb, 11, 4, 2))
        );
        assert_eq!(parse_server_version(&server("SQLite 3")), None);
    }

    #[test]
    fn pg_dump_must_be_at_least_the_server_release() {
        let server = tv(Flavor::Postgres, 17, 2, 0);
        let check_dump = |found| check(Tool::PgDump, found, server);
        assert_eq!(
            check_dump(tv(Flavor::Postgres, 17, 0, 0)),
            Compatibility::Compatible
        );
        assert_eq!(
            check_dump(tv(Flavor::Postgres, 18, 4, 0)),
            Compatibility::Compatible
        );
        let Compatibility::Incompatible(message) = check_dump(tv(Flavor::Postgres, 16, 9, 0))
        else {
            panic!("pg_dump 16 must not dump a 17 server");
        };
        assert!(message.contains("pg_dump 17 or newer"), "{message}");
    }

    #[test]
    fn pre_10_postgres_releases_use_two_numbers() {
        let server = tv(Flavor::Postgres, 9, 6, 1);
        assert!(matches!(
            check(Tool::PgDump, tv(Flavor::Postgres, 9, 5, 9), server),
            Compatibility::Incompatible(message) if message.contains("9.6 or newer")
        ));
        assert_eq!(
            check(Tool::PgDump, tv(Flavor::Postgres, 9, 6, 0), server),
            Compatibility::Compatible
        );
    }

    #[test]
    fn older_psql_and_pg_restore_only_warn() {
        let server = tv(Flavor::Postgres, 17, 2, 0);
        let old = tv(Flavor::Postgres, 15, 0, 0);
        assert!(matches!(
            check(Tool::Psql, old, server),
            Compatibility::Warning(_)
        ));
        assert!(matches!(
            check(Tool::PgRestore, old, server),
            Compatibility::Warning(_)
        ));
    }

    #[test]
    fn mysql_mismatches_warn() {
        let server = tv(Flavor::Mysql, 8, 4, 3);
        assert_eq!(
            check(Tool::Mysqldump, tv(Flavor::Mysql, 8, 4, 0), server),
            Compatibility::Compatible
        );
        assert!(matches!(
            check(Tool::Mysqldump, tv(Flavor::Mysql, 8, 0, 36), server),
            Compatibility::Warning(_)
        ));
        assert!(matches!(
            check(Tool::Mysqldump, tv(Flavor::Mariadb, 11, 4, 2), server),
            Compatibility::Warning(message) if message.contains("MariaDB")
        ));
    }

    #[test]
    fn tools_of_another_engine_are_incompatible() {
        assert!(matches!(
            check(
                Tool::PgDump,
                tv(Flavor::Postgres, 17, 0, 0),
                tv(Flavor::Mysql, 8, 4, 0)
            ),
            Compatibility::Incompatible(_)
        ));
    }

    #[cfg(unix)]
    mod detection {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn temp_dir(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "savoia-tools-{name}-{}",
                savoia_core::ConnectionId::new()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// A fake executable that prints `version_output` for `--version`.
        fn fake_tool(dir: &Path, program: &str, version_output: &str) {
            let path = dir.join(program);
            std::fs::write(&path, format!("#!/bin/sh\necho '{version_output}'\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        #[test]
        fn picks_the_newest_copy() {
            let old = temp_dir("old");
            let new = temp_dir("new");
            fake_tool(&old, "pg_dump", "pg_dump (PostgreSQL) 15.4");
            fake_tool(&new, "pg_dump", "pg_dump (PostgreSQL) 17.2");
            let search = ToolSearch {
                custom_dir: None,
                dirs: vec![old.clone(), new.clone(), temp_dir("empty")],
            };
            let found = detect(Tool::PgDump, &search).unwrap();
            assert_eq!(found.path, new.join("pg_dump"));
            assert_eq!(found.version, tv(Flavor::Postgres, 17, 2, 0));
        }

        #[test]
        fn the_custom_directory_wins_over_newer_copies() {
            let custom = temp_dir("custom");
            let other = temp_dir("other");
            fake_tool(&custom, "pg_dump", "pg_dump (PostgreSQL) 16.1");
            fake_tool(&other, "pg_dump", "pg_dump (PostgreSQL) 18.0");
            let search = ToolSearch {
                custom_dir: Some(custom.clone()),
                dirs: vec![other],
            };
            assert_eq!(
                detect(Tool::PgDump, &search).unwrap().path,
                custom.join("pg_dump")
            );
        }

        #[test]
        fn falls_back_when_the_custom_directory_lacks_the_tool() {
            let other = temp_dir("fallback");
            fake_tool(&other, "psql", "psql (PostgreSQL) 17.0");
            let search = ToolSearch {
                custom_dir: Some(temp_dir("missing")),
                dirs: vec![other.clone()],
            };
            assert_eq!(
                detect(Tool::Psql, &search).unwrap().path,
                other.join("psql")
            );
        }

        #[test]
        fn finds_mariadb_names_and_skips_impostors() {
            let dir = temp_dir("mariadb");
            fake_tool(&dir, "mysql", "not a database client");
            fake_tool(
                &dir,
                "mariadb",
                "mariadb from 11.4.2-MariaDB, client 15.2 for Linux",
            );
            fake_tool(&dir, "psql", "mysql  Ver 8.4.3 for Linux");
            let search = ToolSearch {
                custom_dir: None,
                dirs: vec![dir.clone()],
            };
            let found = detect(Tool::Mysql, &search).unwrap();
            assert_eq!(found.path, dir.join("mariadb"));
            assert_eq!(found.version.flavor, Flavor::Mariadb);
            assert_eq!(detect(Tool::Psql, &search), None);
        }

        #[test]
        fn nothing_found_is_none() {
            let search = ToolSearch {
                custom_dir: None,
                dirs: vec![temp_dir("nothing")],
            };
            assert_eq!(detect(Tool::Mysqldump, &search), None);
        }
    }
}
