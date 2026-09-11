//! Minimal execution-platform pkg-config for Kanna's locked Linux sysroots.
//!
//! Build scripts only need the query surface used by Rust's `pkg-config`
//! crate. Keeping the reader in the Bazel graph avoids consulting a host
//! pkg-config executable or its default search paths while cross compiling.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
struct Package {
    variables: BTreeMap<String, String>,
    fields: BTreeMap<String, String>,
}

fn expand(mut value: String, variables: &BTreeMap<String, String>) -> Result<String, String> {
    for _ in 0..64 {
        let Some(start) = value.find("${") else {
            return Ok(value);
        };
        let end = value[start + 2..]
            .find('}')
            .map(|offset| start + 2 + offset)
            .ok_or_else(|| format!("unterminated variable in {value}"))?;
        let name = &value[start + 2..end];
        let replacement = variables
            .get(name)
            .ok_or_else(|| format!("undefined pkg-config variable {name}"))?;
        value.replace_range(start..=end, replacement);
    }
    Err("pkg-config variable expansion exceeded 64 substitutions".into())
}

fn parse_pc(contents: &str, sysroot: &str) -> Result<Package, String> {
    let mut package = Package::default();
    package
        .variables
        .insert("pc_sysrootdir".into(), sysroot.into());
    let mut logical = String::new();
    for raw in contents.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        logical.push_str(line.strip_suffix('\\').unwrap_or(line));
        if line.ends_with('\\') {
            continue;
        }
        if let Some((name, value)) = logical.split_once('=') {
            if !name.contains(':') {
                let expanded = expand(value.trim().into(), &package.variables)?;
                package.variables.insert(name.trim().into(), expanded);
                logical.clear();
                continue;
            }
        }
        let (name, value) = logical
            .split_once(':')
            .ok_or_else(|| format!("invalid pkg-config line: {logical}"))?;
        let expanded = expand(value.trim().into(), &package.variables)?;
        package.fields.insert(name.trim().into(), expanded);
        logical.clear();
    }
    if !logical.is_empty() {
        return Err("unterminated pkg-config continuation".into());
    }
    Ok(package)
}

fn package_name(requirement: &str) -> Option<&str> {
    requirement
        .split_whitespace()
        .next()
        .filter(|name| !name.is_empty())
}

fn requirements(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter_map(package_name)
        .map(str::to_owned)
        .collect()
}

struct Database {
    search: Vec<PathBuf>,
    sysroot: String,
    packages: BTreeMap<String, Package>,
}

impl Database {
    fn new(search: Vec<PathBuf>, sysroot: String) -> Self {
        Self {
            search,
            sysroot,
            packages: BTreeMap::new(),
        }
    }

    fn load(&mut self, name: &str, include_private: bool) -> Result<(), String> {
        if self.packages.contains_key(name) {
            return Ok(());
        }
        let path = self
            .search
            .iter()
            .map(|dir| dir.join(format!("{name}.pc")))
            .find(|path| path.is_file())
            .ok_or_else(|| format!("package {name} was not found in PKG_CONFIG_LIBDIR"))?;
        let package = parse_pc(
            &fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?,
            &self.sysroot,
        )?;
        let mut deps = requirements(
            package
                .fields
                .get("Requires")
                .map(String::as_str)
                .unwrap_or(""),
        );
        if include_private {
            deps.extend(requirements(
                package
                    .fields
                    .get("Requires.private")
                    .map(String::as_str)
                    .unwrap_or(""),
            ));
        }
        self.packages.insert(name.into(), package);
        for dependency in deps {
            self.load(&dependency, include_private)?;
        }
        Ok(())
    }

    fn flags(
        &self,
        root: &str,
        static_link: bool,
        include_cflags: bool,
        include_libs: bool,
    ) -> Result<String, String> {
        let mut visited = BTreeSet::new();
        let mut output = Vec::new();
        self.collect_flags(
            root,
            static_link,
            include_cflags,
            include_libs,
            &mut visited,
            &mut output,
        )?;
        Ok(output.join(" "))
    }

    fn collect_flags(
        &self,
        name: &str,
        static_link: bool,
        include_cflags: bool,
        include_libs: bool,
        visited: &mut BTreeSet<String>,
        output: &mut Vec<String>,
    ) -> Result<(), String> {
        if !visited.insert(name.into()) {
            return Ok(());
        }
        let package = self
            .packages
            .get(name)
            .ok_or_else(|| format!("package {name} is not loaded"))?;
        let mut fields = Vec::new();
        if include_cflags {
            fields.push("Cflags");
        }
        if include_libs {
            fields.push("Libs");
        }
        for field in fields {
            if let Some(value) = package.fields.get(field) {
                output.extend(value.split_whitespace().map(|flag| self.sysroot_flag(flag)));
            }
        }
        if static_link && include_libs {
            if let Some(value) = package.fields.get("Libs.private") {
                output.extend(value.split_whitespace().map(|flag| self.sysroot_flag(flag)));
            }
        }
        for dependency in requirements(
            package
                .fields
                .get("Requires")
                .map(String::as_str)
                .unwrap_or(""),
        ) {
            self.collect_flags(
                &dependency,
                static_link,
                include_cflags,
                include_libs,
                visited,
                output,
            )?;
        }
        if static_link {
            for dependency in requirements(
                package
                    .fields
                    .get("Requires.private")
                    .map(String::as_str)
                    .unwrap_or(""),
            ) {
                self.collect_flags(
                    &dependency,
                    static_link,
                    include_cflags,
                    include_libs,
                    visited,
                    output,
                )?;
            }
        }
        Ok(())
    }

    fn sysroot_flag(&self, flag: &str) -> String {
        for prefix in ["-I", "-L"] {
            if let Some(path) = flag.strip_prefix(prefix) {
                if path.starts_with('/') && !path.starts_with(&self.sysroot) {
                    return format!("{prefix}{}{path}", self.sysroot);
                }
            }
        }
        flag.into()
    }
}

fn normalize_sysroot(value: String) -> String {
    value.replace("/.kanna-sysroot/..", "")
}

fn run() -> Result<String, String> {
    let args: Vec<String> = env::args().skip(1).collect();
    let static_link = args.iter().any(|arg| arg == "--static");
    let modversion = args.iter().any(|arg| arg == "--modversion");
    let include_cflags = args.iter().any(|arg| arg == "--cflags");
    let include_libs = args.iter().any(|arg| arg == "--libs");
    let name = args
        .iter()
        .rev()
        .find(|arg| !arg.starts_with('-') && !arg.contains(['<', '>', '=']))
        .and_then(|arg| package_name(arg))
        .ok_or_else(|| "pkg-config query has no package name".to_string())?;
    let sysroot = normalize_sysroot(
        env::var("PKG_CONFIG_SYSROOT_DIR")
            .map_err(|_| "PKG_CONFIG_SYSROOT_DIR is required".to_string())?,
    );
    let libdir =
        env::var("PKG_CONFIG_LIBDIR").map_err(|_| "PKG_CONFIG_LIBDIR is required".to_string())?;
    let search = env::split_paths(&libdir)
        .map(|path| Path::new(&normalize_sysroot(path.to_string_lossy().into())).to_path_buf())
        .collect();
    let mut database = Database::new(search, sysroot);
    database.load(name, static_link)?;
    if modversion {
        database.packages[name]
            .fields
            .get("Version")
            .cloned()
            .ok_or_else(|| format!("package {name} has no Version"))
    } else {
        database.flags(name, static_link, include_cflags, include_libs)
    }
}

fn main() {
    match run() {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("kanna pkg-config: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_variables_and_prefixes_target_paths() {
        let package = parse_pc(
            "prefix=/usr\nlibdir=${prefix}/lib\nVersion: 1.2.3\nLibs: -L${libdir} -lthing\nCflags: -I${prefix}/include\n",
            "/target",
        )
        .unwrap();
        let mut database = Database::new(Vec::new(), "/target".into());
        database.packages.insert("thing".into(), package);
        assert_eq!(
            database.flags("thing", false, true, true).unwrap(),
            "-I/target/usr/include -L/target/usr/lib -lthing"
        );
    }

    #[test]
    fn marker_parent_form_normalizes_without_reading_the_host() {
        assert_eq!(
            normalize_sysroot("/execroot/external/repo/sysroot/.kanna-sysroot/..".into()),
            "/execroot/external/repo/sysroot"
        );
    }
}
