//! Dimensioned quantities with strict parsing and canonical units.
//!
//! Accepted input forms (YAML/JSON):
//!   `"300 K"`            — string form
//!   `{value: 300, unit: K}` — map form
//! Canonical storage is `{"value": f64, "unit": canonical}` so cache keys and
//! fingerprints never depend on surface syntax (plan §32).

use crate::error::{M3FlowError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Temperature,
    Pressure,
    Time,
    Length,
    Density,
    Energy,
    Area,
}

impl Dimension {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "temperature" => Some(Self::Temperature),
            "pressure" => Some(Self::Pressure),
            "time" => Some(Self::Time),
            "length" => Some(Self::Length),
            "density" => Some(Self::Density),
            "energy" => Some(Self::Energy),
            "area" => Some(Self::Area),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Temperature => "temperature",
            Self::Pressure => "pressure",
            Self::Time => "time",
            Self::Length => "length",
            Self::Density => "density",
            Self::Energy => "energy",
            Self::Area => "area",
        }
    }

    pub fn canonical_unit(&self) -> &'static str {
        match self {
            Self::Temperature => "K",
            Self::Pressure => "bar",
            Self::Time => "fs",
            Self::Length => "angstrom",
            Self::Density => "g/cm3",
            Self::Energy => "kcal/mol",
            Self::Area => "angstrom2",
        }
    }

    /// (accepted symbol, multiplier to canonical unit)
    pub fn units(&self) -> &'static [(&'static str, f64)] {
        match self {
            Self::Temperature => &[("K", 1.0)],
            Self::Pressure => &[
                ("bar", 1.0),
                ("atm", 1.01325),
                ("Pa", 1.0e-5),
                ("kPa", 1.0e-4),
                ("MPa", 10.0),
                ("GPa", 10000.0),
                ("psi", 0.0689476),
            ],
            Self::Time => &[
                ("fs", 1.0),
                ("ps", 1.0e3),
                ("ns", 1.0e6),
                ("us", 1.0e9),
                ("s", 1.0e15),
                ("min", 60.0e15),
                ("h", 3600.0e15),
            ],
            Self::Length => &[("angstrom", 1.0), ("A", 1.0), ("nm", 10.0)],
            Self::Density => &[("g/cm3", 1.0), ("kg/m3", 1.0e-3), ("g/ml", 1.0)],
            Self::Energy => &[
                ("kcal/mol", 1.0),
                ("kJ/mol", 0.2390057),
                ("eV", 23.06055),
                ("kJ", 0.2390057),
                ("kcal", 1.0),
            ],
            Self::Area => &[
                ("angstrom2", 1.0),
                ("A2", 1.0),
                ("nm2", 100.0),
                ("m2", 1.0e20),
            ],
        }
    }

    fn factor(&self, unit: &str) -> Option<f64> {
        self.units()
            .iter()
            .find(|(u, _)| *u == unit)
            .map(|(_, f)| *f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quantity {
    pub value: f64,
    pub dimension: Dimension,
    /// Canonical unit for the dimension (see `Dimension::canonical_unit`).
    pub unit: &'static str,
}

impl Quantity {
    pub fn new(dimension: Dimension, value: f64) -> Self {
        Self {
            value,
            dimension,
            unit: dimension.canonical_unit(),
        }
    }

    /// Parse any accepted surface form into canonical units.
    pub fn parse_json(dim: Dimension, v: &serde_json::Value) -> Result<Self> {
        match v {
            serde_json::Value::String(s) => Self::parse_str(dim, s),
            serde_json::Value::Object(m) => {
                let value = m
                    .get("value")
                    .and_then(|x| x.as_f64().or_else(|| x.as_i64().map(|i| i as f64)))
                    .ok_or_else(|| M3FlowError::schema("quantity object needs numeric 'value'"))?;
                let unit = m
                    .get("unit")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| M3FlowError::schema("quantity object needs string 'unit'"))?;
                Self::convert(dim, value, unit)
            }
            serde_json::Value::Number(_) => Err(M3FlowError::schema(format!(
                "bare number given for {} parameter; a unit is required (e.g. \"{}\")",
                dim.as_str(),
                dim.canonical_unit()
            ))),
            _ => Err(M3FlowError::schema(format!(
                "cannot parse {} quantity from {}",
                dim.as_str(),
                v
            ))),
        }
    }

    pub fn parse_str(dim: Dimension, s: &str) -> Result<Self> {
        let s = s.trim();
        let split = s
            .find(|c: char| c.is_alphabetic() || c == '/' || c == '°')
            .ok_or_else(|| M3FlowError::schema(format!("missing unit in quantity '{s}'")))?;
        let (num, unit) = s.split_at(split);
        let value: f64 = num
            .trim()
            .parse()
            .map_err(|_| M3FlowError::schema(format!("invalid numeric part in quantity '{s}'")))?;
        Self::convert(dim, value, unit.trim())
    }

    pub fn convert(dim: Dimension, value: f64, unit: &str) -> Result<Self> {
        let factor = dim.factor(unit).ok_or_else(|| {
            let known: Vec<_> = dim.units().iter().map(|(u, _)| *u).collect();
            M3FlowError::schema(format!(
                "unknown unit '{unit}' for {}; accepted: {}",
                dim.as_str(),
                known.join(", ")
            ))
        })?;
        Ok(Self::new(dim, value * factor))
    }

    /// Canonical JSON value: `{"value": x, "unit": "K"}`.
    pub fn canonical_json(&self) -> serde_json::Value {
        serde_json::json!({"value": self.value, "unit": self.unit})
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.value, self.unit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_forms() {
        let q = Quantity::parse_str(Dimension::Temperature, "300 K").unwrap();
        assert_eq!(q.value, 300.0);
        let q = Quantity::parse_str(Dimension::Time, "5 ns").unwrap();
        assert_eq!(q.value, 5.0e6);
        assert_eq!(q.unit, "fs");
        let q = Quantity::parse_str(Dimension::Pressure, "1000 atm").unwrap();
        assert!((q.value - 1013.25).abs() < 1e-6);
        assert_eq!(q.unit, "bar");
    }

    #[test]
    fn rejects_bare_numbers_and_bad_units() {
        assert!(Quantity::parse_json(Dimension::Temperature, &serde_json::json!(300)).is_err());
        assert!(Quantity::parse_str(Dimension::Pressure, "1 furlong").is_err());
    }

    #[test]
    fn map_form() {
        let q = Quantity::parse_json(
            Dimension::Density,
            &serde_json::json!({"value": 850, "unit": "kg/m3"}),
        )
        .unwrap();
        assert!((q.value - 0.85).abs() < 1e-12);
        assert_eq!(q.unit, "g/cm3");
    }
}
