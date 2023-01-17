use super::boot_loader::MenuEntry;
use super::fs::FsType;
use crate::boot::boot_loader::{BootLoader, Error};
use crate::boot::util::*;
use clap::{ArgAction, Parser};
use grub::{GrubEntry, GrubEnvironment, GrubEvaluator};
use log::debug;
use std::io::Read;
use std::path::PathBuf;
use std::{collections::HashMap, fs, path::Path};

const GRUB_ENVIRONMENT_BLOCK_LENGTH: i32 = 1024;
const GRUB_ENVIRONMENT_BLOCK_HEADER: &str = r#"# GRUB Environment Block
# WARNING: Do not edit this file by tools other than grub-editenv!!!"#;

fn grub_environment_block(env: Vec<(String, String)>) -> Result<String, String> {
    let mut block = String::new();
    block.push_str(GRUB_ENVIRONMENT_BLOCK_HEADER);
    block.push('\n');
    for (name, value) in env {
        let line = format!("{name}={value}\n");
        block.push_str(line.as_str());
    }
    let fill_len = GRUB_ENVIRONMENT_BLOCK_LENGTH - block.len() as i32;
    if fill_len < 0 {
        Err("environment block too large".to_string())
    } else {
        block.push_str("#".repeat(fill_len.try_into().unwrap()).as_str());
        Ok(block)
    }
}

fn load_env(contents: impl Into<String>, whitelisted_vars: Vec<String>) -> Vec<(String, String)> {
    contents
        .into()
        .lines()
        .filter(|line| !line.starts_with('#'))
        .fold(Vec::new(), |mut acc, curr| {
            if let Some(split) = curr.split_once('=') {
                if whitelisted_vars.is_empty() || whitelisted_vars.iter().any(|a| a == split.0) {
                    acc.push((split.0.to_string(), split.1.to_string()));
                }
            }
            acc
        })
}

struct TinybootGrubEnvironment {
    env: HashMap<String, String>,
}

// https://www.gnu.org/software/grub/manual/grub/grub.html#search
#[derive(Parser)]
struct SearchArgs {
    #[arg(short = 'f', long, default_value_t = false)]
    file: bool,
    #[arg(short = 'l', long, default_value_t = false)]
    label: bool,
    #[arg(short = 'u', long, default_value_t = false)]
    fs_uuid: bool,
    #[arg(long, default_value = None)]
    set: Option<String>,
    #[arg(long, default_value_t = false)]
    no_floppy: bool,
    name: String,
}

#[derive(Parser)]
struct LoadEnvArgs {
    #[arg(long, value_parser, default_value = "grubenv")]
    file: PathBuf,
    #[arg(default_value_t = false)]
    skip_sig: bool,
    #[arg(action = ArgAction::Append)]
    whitelisted_variables: Vec<String>,
}

#[derive(Parser)]
struct SaveEnvArgs {
    #[arg(long, value_parser, default_value = "grubenv")]
    file: PathBuf,
    #[arg(action = ArgAction::Append)]
    variables: Vec<String>,
}

impl TinybootGrubEnvironment {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            env: HashMap::from([
                ("?".to_string(), 0.to_string()),
                ("prefix".to_string(), prefix.into()),
                ("grub_platform".to_string(), "tinyboot".to_string()),
            ]),
        }
    }

    // TODO(jared): the docs mention being able to load multiple initrds, but what is the use case
    // for that?
    // https://www.gnu.org/software/grub/manual/grub/html_node/initrd.html#initrd
    fn run_initrd(&mut self, args: Vec<String>) -> u8 {
        let mut args = args.iter();
        let Some(initrd) = args.next() else { return 1; };
        self.env.insert("initrd".to_string(), initrd.to_string());
        0
    }

    fn run_linux(&mut self, args: Vec<String>) -> u8 {
        let mut args = args.iter();
        let Some(kernel) = args.next() else { return 1; };
        let mut cmdline = String::new();
        for next in args {
            cmdline.push_str(next);
            cmdline.push(' ');
        }
        self.env.insert("linux".to_string(), kernel.to_string());
        self.env.insert("linux_cmdline".to_string(), cmdline);
        0
    }

    fn run_load_env(&mut self, args: Vec<String>) -> u8 {
        let args = LoadEnvArgs::parse_from(args);

        let Some(prefix) = self.env.get("prefix") else {
            debug!("no prefix environment variable");
            return 1;
        };

        let prefix = PathBuf::from(prefix);
        let Ok(mut file) = fs::File::open(prefix.join(args.file)) else {
            debug!("failed to open env file");
            return 1;
        };

        let mut contents = String::new();
        if file.read_to_string(&mut contents).is_err() {
            debug!("could not read env file");
            return 1;
        };

        let env = load_env(contents, args.whitelisted_variables);
        self.env.extend(env);

        0
    }

    fn run_save_env(&self, args: Vec<String>) -> u8 {
        let args = SaveEnvArgs::parse_from(args);
        if args.variables.is_empty() {
            return 0;
        }

        let Some(prefix) = self.env.get("prefix") else {
            debug!("no prefix environment variable");
            return 1;
        };
        let prefix = PathBuf::from(prefix);
        let file = prefix.join(args.file);

        let Ok(existing_env_block_contents) = fs::read_to_string(&file) else {
            debug!("failed to load environment block file");
            return 1;
        };

        let mut envs = load_env(existing_env_block_contents, vec![]);

        for var in args.variables {
            if let Some(value) = self.env.get(&var) {
                envs.push((var.to_string(), value.to_string()));
            }
        }

        let Ok(block) = grub_environment_block(envs) else {
            debug!("could not generate grub environment block");
            return 1;
        };

        if let Err(e) = fs::write(file, block) {
            debug!("write: {e}");
            return 1;
        }

        0
    }

    fn run_search(&mut self, args: Vec<String>) -> u8 {
        let args = SearchArgs::parse_from(args);
        let var = args.set.unwrap_or_else(|| String::from("root"));
        let found = match (args.file, args.fs_uuid, args.label) {
            (true, false, false) => {
                let file = Path::new(&args.name);
                crate::boot::fs::find_block_device(|p| p == file)
            }
            (false, true, false) => {
                crate::boot::fs::find_block_device(|p| match crate::boot::fs::detect_fs_type(p) {
                    Ok(FsType::Ext4(uuid, ..)) => uuid == args.name,
                    Ok(FsType::Vfat(uuid, ..)) => uuid == args.name,
                    _ => false,
                })
            }
            (false, false, true) => {
                crate::boot::fs::find_block_device(|p| match crate::boot::fs::detect_fs_type(p) {
                    Ok(FsType::Ext4(_, label, ..)) => label == args.name,
                    Ok(FsType::Vfat(_, label, ..)) => label == args.name,
                    _ => false,
                })
            }
            _ => return 1,
        };

        let Ok(found) = found else { return 1; };
        if found.is_empty() {
            return 1;
        }

        let Some(value) = found[0].to_str().map(|s| s.to_string()) else { return 1; };
        self.env.insert(var, value);
        0
    }

    fn run_set(&mut self, args: Vec<String>) -> u8 {
        match args.len() {
            0 | 1 => 2,
            2 => match (args[0].as_str(), args[1].as_str()) {
                (key, "=") => {
                    self.env.remove(key);
                    0
                }
                _ => 2,
            },
            3 => match (args[0].as_str(), args[1].as_str(), args[2].as_str()) {
                (key, "=", val) => {
                    self.env.insert(key.to_string(), val.to_string());
                    0
                }
                _ => 2,
            },
            _ => 2,
        }
    }

    /// Returns exit code 0 if the test evaluates to true.
    /// Returns exit code 1 if the test evaluates to false.
    /// Returns exit code 2 if the arguments are invalid.
    fn run_test(&self, args: Vec<String>) -> u8 {
        match args.len() {
            0 => 2,
            1 => string_nonzero_length(&args[0]),
            2 => match (args[0].as_str(), args[1].as_str()) {
                // file exists and is a directory
                ("-d", file) => file_exists_and_is_directory(file),
                // file exists
                ("-e", file) => file_exists(file),
                // file exists and is not a directory
                ("-f", file) => file_exists_and_is_not_directory(file),
                // file exists and has a size greater than zero
                ("-s", file) => file_exists_and_size_greater_than_zero(file),
                // the length of string is nonzero
                ("-n", string) => string_nonzero_length(string),
                // the length of string is zero
                ("-z", string) => string_zero_length(string),
                // expression is false
                ("!", _expression) => todo!("implement 'expression is false'"),
                _ => 2,
            },
            3 => match (args[0].as_str(), args[1].as_str(), args[2].as_str()) {
                // the strings are equal
                (string1, "=", string2) => strings_equal(string1, string2),
                // the strings are equal
                (string1, "==", string2) => strings_equal(string1, string2),
                // the strings are not equal
                (string1, "!=", string2) => strings_not_equal(string1, string2),
                // string1 is lexicographically less than string2
                (string1, "<", string2) => strings_lexographically_less_than(string1, string2),
                // string1 is lexicographically less or equal than string2
                (string1, "<=", string2) => {
                    strings_lexographically_less_than_or_equal_to(string1, string2)
                }
                // string1 is lexicographically greater than string2
                (string1, ">", string2) => strings_lexographically_greater_than(string1, string2),
                // string1 is lexicographically greater or equal than string2
                (string1, ">=", string2) => {
                    strings_lexographically_greater_than_or_equal_to(string1, string2)
                }
                // integer1 is equal to integer2
                (integer1, "-eq", integer2) => integers_equal(integer1, integer2),
                // integer1 is greater than or equal to integer2
                (integer1, "-ge", integer2) => {
                    integers_greater_than_or_equal_to(integer1, integer2)
                }
                // integer1 is greater than integer2
                (integer1, "-gt", integer2) => integers_greater_than(integer1, integer2),
                // integer1 is less than or equal to integer2
                (integer1, "-le", integer2) => integers_less_than_or_equal_to(integer1, integer2),
                // integer1 is less than integer2
                (integer1, "-lt", integer2) => integers_less_than(integer1, integer2),
                // integer1 is not equal to integer2
                (integer1, "-ne", integer2) => integers_not_equal(integer1, integer2),
                // integer1 is greater than integer2 after stripping off common non-numeric prefix.
                (prefixinteger1, "-pgt", prefixinteger2) => {
                    integers_prefix_greater_than(prefixinteger1, prefixinteger2)
                }
                // integer1 is less than integer2 after stripping off common non-numeric prefix.
                (prefixinteger1, "-plt", prefixinteger2) => {
                    integers_prefix_less_than(prefixinteger1, prefixinteger2)
                }
                // file1 is newer than file2 (modification time). Optionally numeric bias may be directly appended to -nt in which case it is added to the first file modification time.
                (file1, "-nt", file2) => file_newer_than(file1, file2),
                // file1 is older than file2 (modification time). Optionally numeric bias may be directly appended to -ot in which case it is added to the first file modification time.
                (file1, "-ot", file2) => file_older_than(file1, file2),
                // both expression1 and expression2 are true
                (_expression1, "-a", _expression2) => {
                    todo!("implement 'both expression1 and expression2 are true'")
                }
                // either expression1 or expression2 is true
                (_expression1, "-o", _expression2) => {
                    todo!("implement 'either expression1 or expression2 is true'")
                }
                // expression is true
                ("(", _expression, ")") => todo!("implement 'expression is true'"),
                _ => 2,
            },
            _ => 2,
        }
    }
}

impl GrubEnvironment for TinybootGrubEnvironment {
    fn run_command(&mut self, command: String, args: Vec<String>) -> u8 {
        match command.as_str() {
            "initrd" => self.run_initrd(args),
            "linux" => self.run_linux(args),
            "load_env" => self.run_load_env(args),
            "save_env" => self.run_save_env(args),
            "search" => self.run_search(args),
            "set" => self.run_set(args),
            "test" => self.run_test(args),
            _ => {
                debug!("'{}' not implemented", command);
                0
            }
        }
    }

    fn set_env(&mut self, key: String, val: Option<String>) {
        if let Some(val) = val {
            self.env.insert(key, val);
        } else {
            self.env.remove(&key);
        }
    }

    fn get_env(&self, _key: &str) -> Option<&String> {
        self.env.get(_key)
    }
}

pub struct GrubBootLoader {
    mountpoint: PathBuf,
    evaluator: GrubEvaluator<TinybootGrubEnvironment>,
}

impl GrubBootLoader {
    pub fn new(mountpoint: &Path) -> Result<Self, Error> {
        debug!("creating grub evaluator");
        let evaluator = GrubEvaluator::new(
            fs::File::open(mountpoint.join("boot/grub/grub.cfg"))?,
            TinybootGrubEnvironment::new(mountpoint.to_str().ok_or(Error::InvalidMountpoint)?),
        )
        .map_err(Error::Evaluation)?;

        Ok(Self {
            mountpoint: mountpoint.to_path_buf(),
            evaluator,
        })
    }
}

impl BootLoader for GrubBootLoader {
    fn timeout(&self) -> std::time::Duration {
        self.evaluator.timeout()
    }

    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    fn menu_entries(&self) -> Result<Vec<MenuEntry>, Error> {
        Ok(self
            .evaluator
            .menu
            .iter()
            .filter_map(|entry| {
                // is boot entry
                if entry.consequence.is_some() {
                    Some(MenuEntry::BootEntry((
                        entry.id.as_deref().unwrap_or(entry.title.as_str()),
                        entry.title.as_str(),
                    )))
                }
                // is submenu entry
                else {
                    Some(MenuEntry::SubMenu((
                        entry.id.as_deref().unwrap_or(entry.title.as_str()),
                        entry
                            .menuentries
                            .as_ref()?
                            .iter()
                            .filter_map(|entry| {
                                // ensure this is a boot entry, not a nested submenu (invalid?)
                                entry.consequence.as_ref()?;
                                Some(MenuEntry::BootEntry((
                                    entry.id.as_deref().unwrap_or(entry.title.as_str()),
                                    entry.title.as_str(),
                                )))
                            })
                            .collect(),
                    )))
                }
            })
            .collect())
    }

    /// The entry ID could be the ID or name of a boot entry, submenu, or boot entry nested within
    /// a submenu.
    fn boot_info(
        &mut self,
        entry_id: Option<String>,
    ) -> Result<(&Path, &Path, &str, Option<&Path>), Error> {
        let all_entries = self
            .evaluator
            .menu
            .iter()
            .flat_map(|entry| {
                if let Some(menuentries) = &entry.menuentries {
                    menuentries.clone()
                } else {
                    vec![entry.clone()]
                }
            })
            .collect::<Vec<GrubEntry>>();

        let boot_entry = ('entry: {
            if let Some(entry_id) = entry_id {
                for entry in &all_entries {
                    if entry.consequence.is_some() {
                        if entry.id.as_deref().unwrap_or(entry.title.as_str()) == entry_id {
                            break 'entry Some(entry);
                        }
                    } else if let Some(subentries) = &entry.menuentries {
                        for subentry in subentries {
                            if entry.consequence.is_some()
                                && entry.id.as_deref().unwrap_or(entry.title.as_str()) == entry_id
                            {
                                break 'entry Some(subentry);
                            }
                        }
                    }
                }

                break 'entry None;
            } else {
                let default_entry_idx = self
                    .evaluator
                    .get_env("default")
                    .map(|value| value.parse::<usize>())
                    .unwrap_or(Ok(0usize))
                    .unwrap_or(0usize);

                break 'entry all_entries.get(default_entry_idx);
            }
        })
        .ok_or(Error::BootEntryNotFound)?;

        self.evaluator
            .eval_boot_entry(boot_entry)
            .map_err(Error::Evaluation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grub_run_test() {
        let g = TinybootGrubEnvironment::new("/dev/null");
        assert_eq!(g.run_test(vec!["-d".to_string(), "/dev".to_string()]), 0);
        assert_eq!(g.run_test(vec!["-f".to_string(), "/dev".to_string()]), 1);
        assert_eq!(g.run_test(vec!["-e".to_string(), "/dev".to_string()]), 0);
        assert_eq!(g.run_test(vec!["-n".to_string(), "foo".to_string()]), 0);
        assert_eq!(g.run_test(vec!["-z".to_string(), "foo".to_string()]), 1);
        assert_eq!(g.run_test(vec!["-z".to_string(), "".to_string()]), 0);
        assert_eq!(
            g.run_test(vec![
                "foo1".to_string(),
                "-pgt".to_string(),
                "bar0".to_string()
            ]),
            0
        );
    }

    #[test]
    fn grub_environment_block() {
        let testdata_env_block = include_str!("../../testdata/grubenv");

        let expected = vec![
            ("foo".to_string(), "bar".to_string()),
            ("bar".to_string(), "baz".to_string()),
        ];

        let block = super::grub_environment_block(expected.clone()).unwrap();
        assert_eq!(block, testdata_env_block);

        let env = super::load_env(testdata_env_block, vec![]);
        assert_eq!(env, expected);
    }
}
