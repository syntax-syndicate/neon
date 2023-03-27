//! A serde::Desierialize type for percentages.
//!
//! See [`Value`] for details.

use serde::{Deserialize, Serialize};

/// If the value is not an integer between 0 and 100,
/// deserialization fails with a descriptive error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Value(#[serde(deserialize_with = "deserialize_pct_0_to_100")] u8);

impl Value {
    pub fn get(&self) -> u8 {
        self.0
    }
}

fn deserialize_pct_0_to_100<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let v: u8 = serde::de::Deserialize::deserialize(deserializer)?;
    if v > 100 {
        return Err(serde::de::Error::custom(
            "must be an integer between 0 and 100",
        ));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::Value;

    #[derive(serde::Deserialize, serde::Serialize, Debug, PartialEq, Eq)]
    struct Foo {
        bar: Value,
    }

    #[test]
    fn basics() {
        let input = r#"{ "bar": 50 }"#;
        let foo: Foo = serde_json::from_str(input).unwrap();
        assert_eq!(foo.bar.get(), 50);
    }
    #[test]
    fn null_handling() {
        let input = r#"{ "bar": null }"#;
        let res: Result<Foo, _> = serde_json::from_str(input);
        assert!(res.is_err());
    }
    #[test]
    fn zero() {
        let input = r#"{ "bar": 0 }"#;
        let foo: Foo = serde_json::from_str(input).unwrap();
        assert_eq!(foo.bar.get(), 0);
    }
    #[test]
    fn out_of_range_above() {
        let input = r#"{ "bar": 101 }"#;
        let res: Result<Foo, _> = serde_json::from_str(input);
        assert!(res.is_err());
    }
    #[test]
    fn out_of_range_below() {
        let input = r#"{ "bar": -1 }"#;
        let res: Result<Foo, _> = serde_json::from_str(input);
        assert!(res.is_err());
    }
    #[test]
    fn float() {
        let input = r#"{ "bar": 50.5 }"#;
        let res: Result<Foo, _> = serde_json::from_str(input);
        assert!(res.is_err());
    }
    #[test]
    fn string() {
        let input = r#"{ "bar": "50 %" }"#;
        let res: Result<Foo, _> = serde_json::from_str(input);
        assert!(res.is_err());
    }
}
