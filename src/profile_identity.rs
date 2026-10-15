use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Persistent application/profile identity. Positions, names and runtime names are not IDs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileId([u8; 16]);

impl ProfileId {
    /// Only for unsaved drafts and legacy deserialization; never a stored identity.
    pub const UNSAVED: Self = Self([0; 16]);

    pub fn new() -> io::Result<Self> {
        loop {
            let mut bytes = [0; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
            let id = Self(bytes);
            if id != Self::UNSAVED {
                return Ok(id);
            }
        }
    }

    pub fn is_saved(self) -> bool {
        self != Self::UNSAVED
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ProfileId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("profile ID must contain exactly 32 lowercase hexadecimal digits".into());
        }
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .map_err(|error| error.to_string())?;
        }
        let id = Self(bytes);
        if !id.is_saved() {
            return Err("the all-zero profile ID is reserved for unsaved drafts".into());
        }
        Ok(id)
    }
}

impl Serialize for ProfileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_ambiguous_or_missing_authority() {
        let first = ProfileId::new().unwrap();
        assert_ne!(first, ProfileId::new().unwrap());
        assert_eq!(first.to_string().parse::<ProfileId>().unwrap(), first);
        for invalid in [
            "",
            "0",
            "00000000000000000000000000000000",
            "ABCDEF0123456789abcdef0123456789",
        ] {
            assert!(invalid.parse::<ProfileId>().is_err());
        }
    }
}
