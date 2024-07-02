use std::borrow::Borrow;
use std::collections::HashMap;
use std::fmt::Display;
use std::hash::Hash;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;
use anyhow::{anyhow, Result};
use configparser::ini::{Ini, IniDefault};
use tracing::Level;
use tracing::level_filters::LevelFilter;
use crate::connection::{Destination, PossibleDestinations};

pub(super) trait ConfigSection {
    type Key;
    type Value;

    fn take_value<T, K: ?Sized>(&mut self, key: &K) -> Result<T>
        where
            String: Borrow<K>,
            T: TryFromConfig<Self::Value>,
            K: Hash + Eq + Display;

    fn take_opt_value<T, K: ?Sized>(&mut self, key: &K) -> Option<T>
        where
            String: Borrow<K>,
            T: TryFromConfig<Self::Value>,
            K: Hash + Eq + Display;

    fn take_multi_value<T, K: ?Sized>(&mut self, key: &K) -> Result<Vec<T>>
        where
            String: Borrow<K>,
            T: TryFromConfig<Self::Value>,
            K: Hash + Eq + Display;
}

impl ConfigSection for HashMap<String, Option<String>> {
    type Key = String;
    type Value = String;

    fn take_value<T, K: ?Sized>(&mut self, key: &K) -> Result<T>
        where
            String: Borrow<K>,
            T: TryFromConfig<Self::Value>,
            K: Hash + Eq + Display
    {
        self.remove(key)
            .flatten()
            .map(|v| T::try_from_config(v).ok()).flatten().ok_or(anyhow!("couln't take value of key: {}", key))
    }

    fn take_opt_value<T, K: ?Sized>(&mut self, key: &K) -> Option<T>
        where
            String: Borrow<K>,
            T: TryFromConfig<Self::Value>,
            K: Hash + Eq + Display
    {
        self.remove(key)
            .map(|o| o.map(|v| T::try_from_config(v).ok()))
            .flatten().flatten()
    }

    fn take_multi_value<T, K: ?Sized>(&mut self, key: &K) -> Result<Vec<T>>
    where
        String: Borrow<K>,
        T: TryFromConfig<Self::Value>,
        K: Hash + Eq + Display
    {
        let val_string_opt = self.remove(key).flatten();
        let mut vec = Vec::new();
        let Some(vals_string) = val_string_opt else {
            return Ok(vec);
        };
        for val_str in  vals_string.split_terminator(',') {
            let val_string = val_str.trim().to_owned();
            let val = T::try_from_config(val_string)?;
            vec.push(val);
        }
        Ok(vec)
    }
}

pub(super) trait HasConfigSection {
    type ConfigSection: ConfigSection;

    fn take_section(&mut self, section_name: &str) -> Option<Self::ConfigSection>;

    fn try_take_section(&mut self, section_name: &str) -> Result<Self::ConfigSection>;

    fn get_section(&mut self, section_name: &str, f: fn(&mut Self::ConfigSection) -> Result<()>) -> Result<()>;
}

impl HasConfigSection for HashMap<String, HashMap<String, Option<String>>> {
    type ConfigSection = HashMap<String, Option<String>>;

    fn take_section(&mut self, section_name: &str) -> Option<Self::ConfigSection> {
        self.remove(section_name)
    }

    fn try_take_section(&mut self, section_name: &str) -> Result<Self::ConfigSection> {
        self.remove(section_name).ok_or(anyhow!("section [{}] not found", section_name))
    }

    fn get_section(&mut self, section_name: &str, f: fn(&mut Self::ConfigSection) -> Result<()>) -> Result<()> {
        // Try to take the section, otherwise return error
        let section = self.get_mut(section_name)
            .ok_or(anyhow!("section [{}] not found", section_name))?;
        f(section)
    }
}

pub(super) trait TryFromConfig<T> where Self: Sized {
    fn try_from_config(value: T) -> Result<Self>;
}

impl TryFromConfig<String> for bool {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.parse()?)
    }
}

impl TryFromConfig<String> for IpAddr {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.replace('[', "").replace(']', "").parse()?)
    }
}

impl TryFromConfig<String> for u16 {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.parse()?)
    }
}

impl TryFromConfig<String> for usize {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.parse()?)
    }
}

impl TryFromConfig<String> for Duration {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(Duration::from_secs(value.parse()?))
    }
}

impl TryFromConfig<String> for PathBuf {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.parse()?)
    }
}

impl TryFromConfig<String> for LevelFilter {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.parse()?)
    }
}

impl TryFromConfig<String> for PossibleDestinations {
    /// Try to parse
    fn try_from_config(value: String) -> Result<Self> {
        let (accept, refuse_opt) = value.split_once('|')
            .map(|(accept, refuse)| (accept, Some(refuse)))
            .unwrap_or((value.as_str(), None));
        let possible_destinations = match refuse_opt {
            Some(refuse) => PossibleDestinations::with_else(accept.parse()?, refuse.parse()?),
            None => PossibleDestinations::without_else(accept.parse()?)
        };
        Ok(possible_destinations)
    }
}

impl TryFromConfig<String> for Destination {
    fn try_from_config(value: String) -> Result<Self> {
        Ok(value.parse()?)
    }
}

pub(super) fn get_parser() -> Ini {
    let mut ini_default = IniDefault::default();
    ini_default.default_section = "rusty-valve".to_string();
    ini_default.comment_symbols = vec!['#'];
    ini_default.delimiters = vec!['='];
    Ini::new_from_defaults(ini_default)
}

#[cfg(test)]
mod tests {
    use crate::config::ini_file::TryFromConfig;
    use crate::connection::PossibleDestinations;

    #[test]
    fn parse_possible_destinations() {
        let pass_no_refuse = "test-service".to_string();
        let pass_with_refuse = "[::1],127.0.0.1:4000|test-service".to_string();
        assert!(PossibleDestinations::try_from_config(pass_no_refuse.clone()).ok().is_some(), "this should parse without a refuse destination: {}", pass_no_refuse);
        assert!(PossibleDestinations::try_from_config(pass_with_refuse.clone()).ok().is_some(), "this should parse with a refuse destination: {}", pass_with_refuse);

    }
}